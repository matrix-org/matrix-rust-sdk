use clap::{Args, Subcommand, ValueEnum};
use xshell::cmd;

use crate::{sh, Result};

#[derive(Args)]
pub struct ReleaseArgs {
    #[clap(subcommand)]
    cmd: ReleaseCommand,
}

#[derive(PartialEq, Subcommand)]
enum ReleaseCommand {
    /// Prepare the release of the matrix-sdk workspace.
    ///
    /// This command will update the `README.md`, prepend the `CHANGELOG.md`
    /// file using `git cliff`, and bump the versions in the `Cargo.toml`
    /// files.
    Prepare {
        /// What type of version bump we should perform.
        version: ReleaseVersion,
        /// Actually prepare a release. Dry-run mode is the default.
        #[clap(long)]
        execute: bool,
    },
    /// Publish the release.
    ///
    /// This command will create signed tags, publish the release on crates.io,
    /// and finally push the tags to the repo.
    Publish {
        /// Actually publish a release. Dry-run mode is the default
        #[clap(long)]
        execute: bool,
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
            ReleaseCommand::Prepare { version, execute } => prepare(version, execute),
            ReleaseCommand::Publish { execute } => publish(execute),
            ReleaseCommand::WeeklyReport => weekly_report(),
        }
    }
}

fn check_prerequisites() {
    let sh = sh();

    if cmd!(sh, "cargo release --version").quiet().ignore_stdout().run().is_err() {
        eprintln!("This command requires cargo-release, please install it.");
        eprintln!("More info can be found at: https://github.com/crate-ci/cargo-release?tab=readme-ov-file#install");

        std::process::exit(1);
    }

    if cmd!(sh, "gh version").quiet().ignore_stdout().run().is_err() {
        eprintln!("This command requires GitHub CLI, please install it.");
        eprintln!("More info can be found at: https://cli.github.com/");

        std::process::exit(1);
    }
}

fn prepare(version: ReleaseVersion, execute: bool) -> Result<()> {
    let sh = sh();
    let cmd = cmd!(sh, "cargo release --workspace --no-publish --no-tag --no-push");

    let cmd = if execute { cmd.arg("--execute") } else { cmd };
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

fn publish(execute: bool) -> Result<()> {
    let sh = sh();

    let cmd = cmd!(sh, "cargo release tag --workspace");
    let cmd = if execute { cmd.arg("--execute") } else { cmd };
    cmd.run()?;

    let cmd = cmd!(sh, "cargo release publish --workspace");
    let cmd = if execute { cmd.arg("--execute") } else { cmd };
    cmd.run()?;

    let cmd = cmd!(sh, "cargo release push --workspace");
    let cmd = if execute { cmd.arg("--execute") } else { cmd };
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
    let template = "{{range .}}- {{.title}} by @{{.author.login}}{{\"\\n\\n\"}}{{end}}";
    let template = format!("{header}{template}");

    cmd!(
        sh,
        "gh pr list --search merged:>{one_week_ago} --json {JSON_FIELDS} --template {template}"
    )
    .quiet()
    .run()?;

    Ok(())
}
