use std::env;

use clap::{Args, Subcommand, ValueEnum};
use xshell::{cmd, pushd};

use crate::{workspace, Result};

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
    /// Generate the changelog for a specific crate, this shouldn't be run
    /// manually, cargo-release will call this.
    #[clap(hide = true)]
    Changelog,
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

        // The changelog needs to be generated from the directory of the crate,
        // `cargo-release` changes the directory for us but we need to
        // make sure to not switch back to the workspace dir.
        //
        // More info: https://git-cliff.org/docs/usage/monorepos
        if self.cmd != ReleaseCommand::Changelog {
            let _p = pushd(workspace::root_path()?)?;
        }

        match self.cmd {
            ReleaseCommand::Prepare { version, execute } => prepare(version, execute),
            ReleaseCommand::Publish { execute } => publish(execute),
            ReleaseCommand::WeeklyReport => weekly_report(),
            ReleaseCommand::Changelog => changelog(),
        }
    }
}

fn check_prerequisites() {
    if cmd!("cargo release --version").echo_cmd(false).ignore_stdout().run().is_err() {
        eprintln!("This command requires cargo-release, please install it.");
        eprintln!("More info can be found at: https://github.com/crate-ci/cargo-release?tab=readme-ov-file#install");

        std::process::exit(1);
    }

    if cmd!("git cliff --version").echo_cmd(false).ignore_stdout().run().is_err() {
        eprintln!("This command requires git-cliff, please install it.");
        eprintln!("More info can be found at: https://git-cliff.org/docs/installation/");

        std::process::exit(1);
    }
}

fn prepare(version: ReleaseVersion, execute: bool) -> Result<()> {
    let cmd = cmd!("cargo release --no-publish --no-tag --no-push");

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
    let cmd = cmd!("cargo release tag");
    let cmd = if execute { cmd.arg("--execute") } else { cmd };
    cmd.run()?;

    let cmd = cmd!("cargo release publish");
    let cmd = if execute { cmd.arg("--execute") } else { cmd };
    cmd.run()?;

    let cmd = cmd!("cargo release push");
    let cmd = if execute { cmd.arg("--execute") } else { cmd };
    cmd.run()?;

    Ok(())
}

fn weekly_report() -> Result<()> {
    let lines = cmd!("git log --pretty=format:%H --since='1 week ago'").read()?;

    let Some(start) = lines.split_whitespace().last() else {
        panic!("Could not find a start range for the git commit range.")
    };

    cmd!("git cliff --config cliff-weekly-report.toml {start}..HEAD").run()?;

    Ok(())
}

/// Generate the changelog for a given crate.
///
/// This will be called by `cargo-release` and it will set the correct
/// environment and call it from within the correct directory.
fn changelog() -> Result<()> {
    let dry_run = env::var("DRY_RUN").map(|dry| str::parse::<bool>(&dry)).unwrap_or(Ok(true))?;
    let crate_name = env::var("CRATE_NAME").expect("CRATE_NAME must be set");
    let new_version = env::var("NEW_VERSION").expect("NEW_VERSION must be set");

    if dry_run {
        println!(
            "\nGenerating a changelog for {} (dry run), the following output will be prepended to the CHANGELOG.md file:\n",
            crate_name
        );
    } else {
        println!("Generating a changelog for {}.", crate_name);
    }

    let command = cmd!("git cliff")
        .arg("cliff")
        .arg("--config")
        .arg("../../cliff.toml")
        .arg("--include-path")
        .arg(format!("crates/{}/**/*", crate_name))
        .arg("--repository")
        .arg("../../")
        .arg("--unreleased")
        .arg("--tag")
        .arg(&new_version);

    let command = if dry_run { command } else { command.arg("--prepend").arg("CHANGELOG.md") };

    command.run()?;

    Ok(())
}
