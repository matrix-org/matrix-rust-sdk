use clap::{Args, Subcommand, ValueEnum};
use xshell::{Cmd, cmd};

use crate::{Result, sh};

#[derive(Args)]
pub struct ReleaseArgs {
    #[clap(subcommand)]
    cmd: ReleaseCommand,
}

#[derive(PartialEq, Subcommand)]
enum ReleaseCommand {
    /// Prepare the release of the matrix-sdk workspace.
    ///
    /// This command will update the `README.md`, update the `CHANGELOG.md` file
    /// using, and bump the versions in the `Cargo.toml` files.
    Prepare {
        /// What type of version bump we should perform.
        version: ReleaseVersion,
        /// Actually prepare a release. Dry-run mode is the default.
        #[clap(long)]
        execute: bool,
        /// The crate or package that should be released. Use this if you'd like
        /// to release only one specific crate. The default is to
        /// release all crates.
        #[clap(long)]
        package: Option<String>,
    },
    /// Publish the release.
    ///
    /// This command will create signed tags, publish the release on crates.io,
    /// and finally push the tags to the repo.
    Publish {
        /// Actually publish a release. Dry-run mode is the default
        #[clap(long)]
        execute: bool,
        /// The crate or package that should be released. Use this if you'd like
        /// to release only one specific crate. The default is to
        /// release all crates.
        #[clap(long)]
        package: Option<String>,
    },
    /// Get a list of interesting changes that happened in the last week.
    WeeklyReport,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, ValueEnum)]
enum ReleaseVersion {
    // TODO: Add a Major variant here once we're stable.
    /// Create a new minor release.
    #[default]
    Minor,
    /// Create a new patch release.
    Patch,
    /// Create a new release candidate.
    Rc,
}

impl ReleaseVersion {
    fn as_str(&self) -> &str {
        match self {
            ReleaseVersion::Minor => "minor",
            ReleaseVersion::Patch => "patch",
            ReleaseVersion::Rc => "rc",
        }
    }
}

impl ReleaseArgs {
    pub fn run(self) -> Result<()> {
        check_prerequisites();

        match self.cmd {
            ReleaseCommand::Prepare { version, execute, package } => {
                prepare(version, execute, package)
            }
            ReleaseCommand::Publish { execute, package } => publish(execute, package),
            ReleaseCommand::WeeklyReport => weekly_report(),
        }
    }
}

fn check_prerequisites() {
    let sh = sh();

    if cmd!(sh, "cargo release --version").quiet().ignore_stdout().run().is_err() {
        eprintln!("This command requires cargo-release, please install it.");
        eprintln!(
            "More info can be found at: \
             https://github.com/crate-ci/cargo-release?tab=readme-ov-file#install"
        );

        std::process::exit(1);
    }

    if cmd!(sh, "gh version").quiet().ignore_stdout().run().is_err() {
        eprintln!("This command requires GitHub CLI, please install it.");
        eprintln!("More info can be found at: https://cli.github.com/");

        std::process::exit(1);
    }
}

fn append_options<'a>(command: Cmd<'a>, execute: &bool, package: &Option<String>) -> Cmd<'a> {
    let command = if *execute { command.arg("--execute") } else { command };
    if let Some(package) = package.as_deref() {
        command.args(["--package", package])
    } else {
        command.arg("--workspace")
    }
}

fn prepare(version: ReleaseVersion, execute: bool, package: Option<String>) -> Result<()> {
    let sh = sh();

    let cmd = cmd!(sh, "cargo release --no-publish --no-tag --no-push");
    let cmd = append_options(cmd, &execute, &package);

    let cmd = cmd.arg(version.as_str());

    cmd.run()?;

    if execute {
        eprintln!(
            "Please double check the changelogs and edit them if necessary, \
             publish the PR, and once it's merged, switch to `main` and pull the PR \
             and run `cargo xtask release publish`"
        );
    }

    Ok(())
}

fn publish(execute: bool, package: Option<String>) -> Result<()> {
    let sh = sh();

    let cmd = cmd!(sh, "cargo release tag");
    let cmd = append_options(cmd, &execute, &package);
    cmd.run()?;

    let cmd = cmd!(sh, "cargo release publish");
    let cmd = append_options(cmd, &execute, &package);
    cmd.run()?;

    let cmd = cmd!(sh, "cargo release push");
    let cmd = append_options(cmd, &execute, &package);
    cmd.run()?;

    Ok(())
}

fn weekly_report() -> Result<()> {
    const JSON_FIELDS: &str = "title,number,url,author";

    let sh = sh();

    let one_week_ago = cmd!(sh, "date -d '1 week ago' +%Y-%m-%d").read()?;
    let today = cmd!(sh, "date +%Y-%m-%d").read()?;

    let _env_pager = sh.push_env("GH_PAGER", "");

    let header = format!("# This Week in the Matrix Rust SDK ({today})\n\n");
    let template = "{{range .}}- {{.title}} ([#{{.number}}](https://github.com/matrix-org/matrix-rust-sdk/pull/{{.number}})){{\"\\n\\n\"}}{{end}}";
    let template = format!("{header}{template}");

    cmd!(
        sh,
        "gh pr list --search merged:>{one_week_ago} --limit 100 --json {JSON_FIELDS} --template {template}"
    )
    .quiet()
    .run()?;

    Ok(())
}
