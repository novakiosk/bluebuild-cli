use std::{
    env, fs,
    path::{self, Path, PathBuf},
};

use blue_build_recipe::{Recipe, RecipeGetters};
use blue_build_utils::{
    constants::{
        ARCHIVE_SUFFIX, BB_GENISO_DISPLAY_NAME, BB_GENISO_ENROLLMENT_PASSWORD,
        BB_GENISO_INTERACTIVE_SETUP, BB_GENISO_ISO_NAME, BB_GENISO_SECURE_BOOT_URL,
        BB_GENISO_WEB_UI, BB_SKIP_VALIDATION, BB_TEMPDIR, JASONN3_INSTALLER_IMAGE,
    },
    platform::Platform,
    string_vec, tempdir, tempdir_in,
};
use bon::Builder;
use clap::{Args, Subcommand, ValueEnum};
use miette::{Context, IntoDiagnostic, Result, bail};
use oci_client::Reference;

use blue_build_process_management::{
    drivers::{Driver, DriverArgs, RunDriver, opts::RunOpts},
    run_volumes,
};

use super::{BlueBuildCommand, build::BuildCommand};

#[derive(Clone, Debug, Builder, Args)]
pub struct GenerateIsoCommand {
    #[command(subcommand)]
    command: GenIsoSubcommand,

    /// The directory to save the resulting ISO file.
    #[arg(short, long)]
    #[builder(into)]
    output_dir: Option<PathBuf>,

    /// The variant of the installer to use.
    ///
    /// The Kinoite variant will ask for a user
    /// and password before installing the OS.
    /// This version is the most stable and is
    /// recommended.
    ///
    /// The Silverblue variant will ask for a user
    /// and password on first boot after the OS
    /// is installed.
    ///
    /// The Server variant is the basic installer
    /// and will ask to setup a user at install time.
    #[arg(short = 'V', long, default_value = "kinoite")]
    variant: GenIsoVariant,

    /// The url to the secure boot public key.
    ///
    /// Defaults to one of UBlue's public key.
    /// It's recommended to change this if your base
    /// image is not from UBlue.
    #[arg(
        long,
        default_value = "https://github.com/ublue-os/bazzite/raw/main/secure_boot.der",
        env = BB_GENISO_SECURE_BOOT_URL
    )]
    #[builder(into)]
    secure_boot_url: String,

    /// The enrollment password for the secure boot
    /// key.
    ///
    /// Default's to UBlue's enrollment password.
    /// It's recommended to change this if your base
    /// image is not from UBlue.
    #[arg(long, default_value = "universalblue", env = BB_GENISO_ENROLLMENT_PASSWORD)]
    #[builder(into)]
    enrollment_password: String,

    /// Override the display name used for branding
    /// in the installer (GRUB menu, Anaconda UI, etc.).
    ///
    /// By default, the display name is derived from
    /// the last segment of the image reference
    /// (e.g. `ghcr.io/octocat/weird-os` -> `weird-os`).
    /// Use this flag to set a custom name
    /// (e.g. `notweird-os`).
    #[arg(long, env = BB_GENISO_DISPLAY_NAME)]
    #[builder(into)]
    display_name: Option<String>,

    /// The name of your ISO image file.
    #[arg(long, env = BB_GENISO_ISO_NAME)]
    #[builder(into)]
    iso_name: Option<String>,

    /// Enable Anaconda WebUI.
    #[arg(long, env = BB_GENISO_WEB_UI)]
    #[builder(default)]
    web_ui: bool,

    /// Restore installer-time network and user/password setup.
    #[arg(long, env = BB_GENISO_INTERACTIVE_SETUP)]
    #[builder(default)]
    interactive_setup: bool,

    /// The location to temporarily store files
    /// while building. If unset, it will use `/tmp`.
    #[arg(long, env = BB_TEMPDIR)]
    tempdir: Option<PathBuf>,

    /// The platform of the final ISO.
    #[arg(long)]
    platform: Option<Platform>,

    #[clap(flatten)]
    #[builder(default)]
    drivers: DriverArgs,
}

#[derive(Debug, Clone, Subcommand)]
pub enum GenIsoSubcommand {
    /// Build an ISO from a remote image.
    Image {
        /// The image ref to create the iso from.
        #[arg()]
        image: String,
    },
    /// Build an ISO from a recipe.
    ///
    /// This will build the image locally first
    /// before creating the ISO. This is a long
    /// process.
    Recipe {
        /// The path to the recipe file for your image.
        #[arg()]
        recipe: PathBuf,

        /// Skips validation of the recipe file.
        #[arg(long, env = BB_SKIP_VALIDATION)]
        skip_validation: bool,
    },
}

#[derive(Debug, Default, Clone, Copy, ValueEnum)]
pub enum GenIsoVariant {
    #[default]
    Kinoite,
    Silverblue,
    Server,
}

impl std::fmt::Display for GenIsoVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::Server => "Server",
                Self::Silverblue => "Silverblue",
                Self::Kinoite => "Kinoite",
            }
        )
    }
}

impl BlueBuildCommand for GenerateIsoCommand {
    fn try_run(&mut self) -> Result<()> {
        Driver::init(self.drivers);

        let image_out_dir = if let Some(ref dir) = self.tempdir {
            tempdir_in(dir)?
        } else {
            tempdir()?
        };

        let output_dir = if let Some(output_dir) = self.output_dir.clone() {
            if output_dir.exists() && !output_dir.is_dir() {
                bail!("The '--output-dir' arg must be a directory");
            }

            if !output_dir.exists() {
                fs::create_dir(&output_dir).into_diagnostic()?;
            }

            path::absolute(output_dir).into_diagnostic()?
        } else {
            env::current_dir().into_diagnostic()?
        };

        let platform = self.platform.unwrap_or_default();

        if let GenIsoSubcommand::Recipe {
            recipe,
            skip_validation,
        } = &self.command
        {
            BuildCommand::builder()
                .recipe(vec![recipe.clone()])
                .archive(image_out_dir.path())
                .maybe_tempdir(self.tempdir.clone())
                .skip_validation(*skip_validation)
                .platform(vec![platform])
                .build()
                .try_run()?;
        }

        let iso_name = self.iso_name.as_ref().map_or("deploy.iso", String::as_str);
        let iso_path = output_dir.join(iso_name);

        if iso_path.exists() {
            fs::remove_file(iso_path).into_diagnostic()?;
        }

        self.build_iso(iso_name, &output_dir, image_out_dir.path(), platform)
    }
}

impl GenerateIsoCommand {
    fn build_iso(
        &self,
        iso_name: &str,
        output_dir: &Path,
        image_out_dir: &Path,
        platform: Platform,
    ) -> Result<()> {
        let interactive_setup_template_dir = self
            .interactive_setup
            .then(|| write_interactive_setup_template(image_out_dir))
            .transpose()?;

        let mut args = string_vec![
            format!("VARIANT={}", self.variant),
            format!("ISO_NAME=build/{iso_name}"),
            "DNF_CACHE=/cache/dnf",
            format!("SECURE_BOOT_KEY_URL={}", self.secure_boot_url),
            format!("ENROLLMENT_PASSWORD={}", self.enrollment_password),
            format!("WEB_UI={}", self.web_ui),
        ];
        let image_out_dir = &image_out_dir.display().to_string();
        let output_dir = &output_dir.display().to_string();
        let mut vols = run_volumes![
            output_dir => "/build-container-installer/build",
            "dnf-cache" => "/cache/dnf/",
        ];
        let interactive_setup_template_host_dir = interactive_setup_template_dir
            .as_ref()
            .map(|path| path.display().to_string());

        if let Some(template_dir) = interactive_setup_template_host_dir.as_deref() {
            args.push(format!(
                "ADDITIONAL_TEMPLATES={INTERACTIVE_SETUP_TEMPLATE_CONTAINER_PATH}"
            ));
            vols.extend(&run_volumes![
                template_dir => INTERACTIVE_SETUP_TEMPLATE_CONTAINER_DIR,
            ]);
        }

        match &self.command {
            GenIsoSubcommand::Image { image } => {
                let image: Reference = image
                    .parse()
                    .into_diagnostic()
                    .with_context(|| format!("Unable to parse image reference {image}"))?;
                let (image_repo, image_name) = {
                    let registry = image.resolve_registry();
                    let repo = image.repository();
                    let image = format!("{registry}/{repo}");

                    let mut image_parts = image.split('/').collect::<Vec<_>>();
                    let image_name = image_parts.pop().unwrap(); // Should be at least 2 elements
                    let image_repo = image_parts.join("/");
                    (image_repo, image_name.to_string())
                };
                let image_tag = image.tag().unwrap_or("latest");
                let version = format!(
                    "VERSION={}",
                    Driver::get_os_version().oci_ref(&image).call()?
                );

                if let Some(display_name) = &self.display_name {
                    args.extend([
                        format!("IMAGE_NAME={display_name}"),
                        format!("IMAGE_SRC=docker://{image_repo}/{image_name}:{image_tag}"),
                        format!("IMAGE_TAG={image_tag}"),
                        version,
                    ]);
                } else {
                    args.extend([
                        format!("IMAGE_NAME={image_name}"),
                        format!("IMAGE_REPO={image_repo}"),
                        format!("IMAGE_TAG={image_tag}"),
                        version,
                    ]);
                }
            }
            GenIsoSubcommand::Recipe {
                recipe,
                skip_validation: _,
            } => {
                let recipe = Recipe::parse(recipe)?;

                args.extend([
                    format!(
                        "IMAGE_SRC=oci-archive:/img_src/{}.{ARCHIVE_SUFFIX}",
                        recipe.get_name().replace('/', "_"),
                    ),
                    format!(
                        "VERSION={}",
                        Driver::get_os_version()
                            .oci_ref(&recipe.base_image_ref()?)
                            .call()?,
                    ),
                ]);

                if let Some(display_name) = &self.display_name {
                    args.push(format!("IMAGE_NAME={display_name}"));
                }
                vols.extend(&run_volumes![
                    image_out_dir => "/img_src/",
                ]);
            }
        }

        // Currently testing local tarball builds
        let opts = RunOpts::builder()
            .image(JASONN3_INSTALLER_IMAGE)
            .privileged(true)
            .platform(platform)
            .remove(true)
            .args(&args)
            .volumes(&vols)
            .build();

        let status = Driver::run(opts)?;

        if !status.success() {
            bail!("Failed to create ISO");
        }
        Ok(())
    }
}

const INTERACTIVE_SETUP_TEMPLATE_CONTAINER_DIR: &str = "/bluebuild-iso";
const INTERACTIVE_SETUP_TEMPLATE_CONTAINER_PATH: &str = "/bluebuild-iso/interactive-setup.tmpl";
const INTERACTIVE_SETUP_TEMPLATE_DIR: &str = ".bluebuild-iso";
const INTERACTIVE_SETUP_TEMPLATE_FILENAME: &str = "interactive-setup.tmpl";
const INTERACTIVE_SETUP_TEMPLATE: &str = r#"mkdir etc/anaconda/conf.d
append etc/anaconda/conf.d/99-bluebuild-interactive-setup.conf "[User Interface]"
append etc/anaconda/conf.d/99-bluebuild-interactive-setup.conf "hidden_spokes ="
append etc/anaconda/conf.d/99-bluebuild-interactive-setup.conf "hidden_webui_pages ="
"#;

fn write_interactive_setup_template(image_out_dir: &Path) -> Result<PathBuf> {
    let template_dir = image_out_dir.join(INTERACTIVE_SETUP_TEMPLATE_DIR);
    fs::create_dir_all(&template_dir)
        .into_diagnostic()
        .wrap_err("Failed to create the interactive setup template directory")?;
    fs::write(
        template_dir.join(INTERACTIVE_SETUP_TEMPLATE_FILENAME),
        INTERACTIVE_SETUP_TEMPLATE,
    )
    .into_diagnostic()
    .wrap_err("Failed to write the interactive setup template")?;

    Ok(template_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_setup_template_is_written_in_run_dir() {
        let image_out_dir = tempdir().expect("temporary image directory should be created");
        let template_dir = write_interactive_setup_template(image_out_dir.path())
            .expect("interactive setup template should be written");
        let template_path = template_dir.join(INTERACTIVE_SETUP_TEMPLATE_FILENAME);

        assert_eq!(
            template_dir,
            image_out_dir.path().join(INTERACTIVE_SETUP_TEMPLATE_DIR)
        );
        assert_eq!(
            fs::read_to_string(template_path).expect("template should be readable"),
            INTERACTIVE_SETUP_TEMPLATE
        );
    }
}
