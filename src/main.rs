//! `cargo harbor` — the command-line face of [`harbor`].
//!
//! The binary is named `cargo-harbor` so Cargo finds it as the subcommand `cargo harbor`.
//! It is a thin shell over the library: parse arguments, build a [`Release`], call one
//! method, report the result. Everything decidable lives in the library, where it can be
//! tested without a registry, a token, or a workflow.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use clap::Subcommand;
use harbor::Release;
use harbor::ReleaseConfig;

/// Release orchestration for a Cargo workspace.
#[derive(Debug, Parser)]
#[command(
    name = "cargo harbor",
    bin_name = "cargo harbor",
    version,
    about = "Compute and drive a workspace's crates.io release",
    long_about = None,
)]
struct Cli {
    /// The workspace root.
    ///
    /// Defaults to the current directory, which is what a workflow step wants: it checks
    /// out the repository and runs there.
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    /// Only consider crates whose name starts with this prefix.
    ///
    /// Empty by default, meaning every crate in the workspace is a candidate. That is the
    /// safe default: a filter that is on silently drops crates from a release. To keep one
    /// crate out, set `publish = false` in its own manifest. Use this when one workspace
    /// holds several families of crates and only one family belongs to this release.
    #[arg(long, global = true, default_value = "")]
    prefix: String,

    /// Run a release that would change the lockfile.
    ///
    /// Off by default: a release should publish what the committed lockfile resolves, not
    /// what a fresh resolution happens to pick today.
    #[arg(long, global = true)]
    no_locked: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print what would be published, in the order it would go out.
    Plan,

    /// Check that a tag names this workspace's version, and that a token is present.
    Check {
        /// The tag being released, e.g. `v0.1.0`.
        tag: Option<String>,

        /// Also compute the publish plan, so an unpublishable workspace fails here rather
        /// than half way through the release.
        #[arg(long)]
        plan: bool,
    },

    /// Publish every crate, in dependency order.
    Publish {
        /// Print what would happen without uploading anything.
        #[arg(long)]
        dry_run: bool,

        /// Seconds to wait between retries when a dependency is not indexed yet.
        #[arg(long, default_value_t = harbor::config::DEFAULT_RETRY_DELAY_SECS)]
        wait: u64,

        /// How many times to retry a crate whose dependency the index has not caught up on.
        #[arg(long, default_value_t = harbor::config::DEFAULT_MAX_ATTEMPTS)]
        attempts: u32,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    // The command's own flags override the defaults; everything else comes from
    // `ReleaseConfig`. The wait/attempts fallbacks below are the defaults themselves, not
    // second copies of the numbers — those used to be literals here *and* in `config.rs`,
    // which is a default that drifts.
    let defaults = ReleaseConfig::default();
    let (wait, attempts, dry_run) = match &cli.command {
        Command::Publish {
            wait,
            attempts,
            dry_run,
        } => (*wait, *attempts, *dry_run),
        _ => (defaults.retry_delay.as_secs(), defaults.max_attempts, false),
    };

    let config = ReleaseConfig {
        crate_prefix: cli.prefix,
        locked: !cli.no_locked,
        dry_run,
        retry_delay: Duration::from_secs(wait),
        max_attempts: attempts,
        ..defaults
    };

    let release = Release::new(&cli.root).with_config(config);

    match run(&cli.command, &release) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // The annotation first, so a reviewer sees the reason on the run summary rather
            // than only inside a collapsed log group.
            harbor::gha::error(&err.to_string());
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: &Command, release: &Release) -> harbor::Result<()> {
    match command {
        Command::Plan => {
            let plan = release.plan()?;
            print_plan(&plan);
            Ok(())
        }

        Command::Check { tag, plan } => {
            if let Some(tag) = tag {
                let version = release.check_tag(tag)?;
                println!("tag {tag} matches workspace version {version}");
            }
            // Always checked: a release that cannot authenticate fails at the first upload,
            // which is a worse moment to find out than before anything is attempted.
            let token = std::env::var(harbor::preflight::TOKEN_VAR).ok();
            harbor::preflight::check_token(token.as_deref())?;

            if *plan {
                // Computing the plan is offline and free, and it is the only thing that
                // catches a cycle, an unpublished dependency or a members pattern that
                // matches nothing — all of which would otherwise surface mid-release.
                let plan = release.plan()?;
                println!(
                    "plan is computable: {} crate(s) to publish, {} left out",
                    plan.len(),
                    plan.excluded().len()
                );
            }
            Ok(())
        }

        Command::Publish { dry_run, .. } => {
            let report = release.publish()?;
            if *dry_run {
                println!(
                    "dry run: {} crate(s) would be published, nothing was uploaded",
                    report.total()
                );
            } else {
                println!(
                    "{} published, {} already on the registry",
                    report.published.len(),
                    report.already_present.len()
                );
            }
            Ok(())
        }
    }
}

/// Prints a plan in the order it would be published.
fn print_plan(plan: &harbor::Plan) {
    if plan.is_empty() {
        println!("nothing to publish");
    } else {
        println!("{} crate(s) to publish, in this order:", plan.len());
        for (i, krate) in plan.order.iter().enumerate() {
            println!("  {:>2}. {} {}", i + 1, krate.name, krate.version);
        }
    }

    if !plan.excluded().is_empty() {
        let excluded = plan.excluded();
        println!("\n{} not published (publish = false):", excluded.len());
        for krate in excluded {
            println!("  - {} {}", krate.name, krate.version);
        }
    }
}
