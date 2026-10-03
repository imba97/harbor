//! `cargo harbor` — the command-line face of [`harbor`].
//!
//! The binary is named `cargo-harbor` so Cargo finds it as the subcommand `cargo harbor`.
//! It is a thin shell over the library: parse arguments, build a [`Release`], call one
//! method, report the result. Everything decidable lives in the library, where it can be
//! tested without a registry, a token, or a workflow.
//!
//! # The subcommand name arrives as an argument
//!
//! `cargo harbor plan` runs `cargo-harbor` with `["harbor", "plan"]`: cargo passes the
//! subcommand name through and does not strip it. So the program has to drop that name
//! itself before parsing, which is what [`without_plugin_name`] does.
//!
//! Modelling it as a clap subcommand instead — a `harbor` variant holding the real one —
//! dispatches correctly but leaks into everything the user sees: the name appears in the
//! subcommand list, and `--help` on a real subcommand stops working, because from clap's
//! point of view the real subcommands belong to `harbor`. One line of argument surgery
//! keeps the interface honest.

use std::ffi::OsString;
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
    name = "cargo-harbor",
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

    /// Publish exactly these crates, in this order, and nothing else.
    ///
    /// Repeat the flag or separate names with commas: `--order a --order b`, or
    /// `--order a,b`. Without it the release is derived from the manifests: every crate they
    /// include, ordered by the dependency graph.
    ///
    /// Use it when the derived answer is wrong. The case it exists for is a tool that ships
    /// as an installed binary: it declares `publish = false` — correctly, it has no business
    /// being anybody's dependency — and then could not release itself through this crate,
    /// because that flag is exactly what makes `publish = false` mean anything.
    ///
    /// The order is still checked against the graph: naming a crate before something it
    /// depends on is refused rather than obeyed.
    #[arg(long, global = true, value_delimiter = ',', value_name = "CRATE")]
    order: Vec<String>,

    /// The subcommand, dispatched from here.
    ///
    /// Required, because a bare `cargo harbor` has nothing to do: printing help and exiting
    /// zero would let a workflow step that lost its arguments look like a successful
    /// release.
    #[command(subcommand)]
    command: Command,
}

/// Drops the plugin name cargo passes through, so clap sees the real command line.
///
/// `cargo harbor plan` invokes `cargo-harbor` with `["harbor", "plan"]`, and cargo does not
/// strip that name. Left in place, clap rejects it — every invocation fails with
/// `unrecognized subcommand 'harbor'`, `--help` included, which makes a missing line of
/// plumbing look like a broken release.
///
/// Only the token immediately after the program name is dropped, and only when it is exactly
/// the plugin's name. Anything else is left for clap to judge, so a real mistake still gets a
/// real error rather than being silently eaten. Taking `argv` as a parameter rather than
/// reading `std::env::args_os` is what lets the tests cover the shapes cargo actually uses.
fn without_plugin_name(argv: Vec<OsString>) -> Vec<OsString> {
    // `harbor` is the name cargo passes. The `.exe` spelling turns up on Windows when the
    // argument came from a path rather than a bare name.
    let is_plugin_name = |arg: &OsString| {
        let arg = arg.to_string_lossy();
        arg == "harbor" || arg == "harbor.exe"
    };

    let mut argv = argv.into_iter();
    let Some(program) = argv.next() else {
        return Vec::new();
    };
    let mut rest: Vec<OsString> = argv.collect();
    if rest.first().is_some_and(is_plugin_name) {
        rest.remove(0);
    }

    let mut out = Vec::with_capacity(rest.len() + 1);
    out.push(program);
    out.extend(rest);
    out
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
    let cli = Cli::parse_from(without_plugin_name(std::env::args_os().collect()));
    let config = config_from(&cli);
    let release = Release::new(&cli.root).with_config(config);

    match run(cli.command, &release) {
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

/// Turns parsed arguments into a configuration.
///
/// Separate from `main` so the mapping can be tested without running a release: a flag that
/// parses but never reaches the config is a flag that silently does nothing, which is the
/// exact failure mode this crate exists to remove.
fn config_from(cli: &Cli) -> ReleaseConfig {
    // The command's own flags override the defaults; everything else comes from
    // `ReleaseConfig`. The wait/attempts fallbacks are the defaults themselves rather than
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

    ReleaseConfig {
        crate_prefix: cli.prefix.clone(),
        locked: !cli.no_locked,
        dry_run,
        retry_delay: Duration::from_secs(wait),
        max_attempts: attempts,
        // Empty means "not asked for", which is not "publish nothing" — the latter would be
        // a release that silently does nothing at all.
        explicit_order: if cli.order.is_empty() {
            None
        } else {
            Some(cli.order.clone())
        },
        ..defaults
    }
}

/// Dispatches a parsed subcommand.
///
/// Takes the command by value: it is the last thing that needs it, and taking a reference
/// would mean cloning the strings out of it for no reason.
fn run(command: Command, release: &Release) -> harbor::Result<()> {
    match command {
        Command::Plan => {
            let plan = release.plan()?;
            print_plan(&plan);
            Ok(())
        }

        Command::Check { tag, plan } => {
            if let Some(tag) = tag {
                let version = release.check_tag(&tag)?;
                println!("tag {tag} matches the manifest version {version}");
            }
            // Always checked: a release that cannot authenticate fails at the first upload,
            // which is a worse moment to find out than before anything is attempted.
            let token = std::env::var(harbor::preflight::TOKEN_VAR).ok();
            harbor::preflight::check_token(token.as_deref())?;

            if plan {
                // Computing the plan is offline and free, and it is the only thing that
                // catches a cycle, an unpublished dependency or a members pattern that
                // matches nothing — all of which would otherwise surface mid-release.
                let planned = release.plan()?;
                println!(
                    "plan is computable: {} crate(s) to publish, {} left out",
                    planned.len(),
                    planned.excluded().len()
                );
            }
            Ok(())
        }

        Command::Publish { dry_run, .. } => {
            let report = release.publish()?;
            if dry_run {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a command line the way `main` would, so the tests exercise clap's
    /// configuration rather than a hand-built `Cli`.
    ///
    /// `harbor` is included in `argv` because that is how cargo invokes a plugin — `cargo
    /// harbor plan` runs `cargo-harbor` with `["harbor", "plan"]` — and then dropped by
    /// [`without_plugin_name`] exactly as it is at runtime. A test that skipped that step
    /// would be testing an invocation that never happens.
    fn parse(args: &[&str]) -> clap::error::Result<Cli> {
        let mut argv: Vec<OsString> = vec!["cargo-harbor".into(), "harbor".into()];
        argv.extend(args.iter().map(OsString::from));
        Cli::try_parse_from(without_plugin_name(argv))
    }

    fn argv(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_plugin_name_cargo_passes_through_is_dropped() {
        // The plumbing, and the failure it prevents: left in place, clap rejects it and
        // every invocation dies with `unrecognized subcommand 'harbor'`, `--help` included.
        assert_eq!(
            without_plugin_name(argv(&["cargo-harbor", "harbor", "plan"])),
            argv(&["cargo-harbor", "plan"])
        );
        // On Windows cargo can pass the `.exe` spelling.
        assert_eq!(
            without_plugin_name(argv(&["cargo-harbor.exe", "harbor.exe", "plan"])),
            argv(&["cargo-harbor.exe", "plan"])
        );
    }

    #[test]
    fn something_that_is_not_the_plugin_name_is_left_alone() {
        // A real mistake has to reach clap and get a real error, rather than being eaten
        // here and turning into a confusing complaint about the next argument.
        assert_eq!(
            without_plugin_name(argv(&["cargo-harbor", "plan"])),
            argv(&["cargo-harbor", "plan"])
        );
        assert_eq!(
            without_plugin_name(argv(&["cargo-harbor", "harbour", "plan"])),
            argv(&["cargo-harbor", "harbour", "plan"])
        );
    }

    #[test]
    fn a_plugin_name_is_only_dropped_once_and_only_at_the_front() {
        // `plan` must survive, and so must a second `harbor` that happens to be an argument.
        assert_eq!(
            without_plugin_name(argv(&["cargo-harbor", "harbor", "plan", "harbor"])),
            argv(&["cargo-harbor", "plan", "harbor"])
        );
    }

    #[test]
    fn an_empty_command_line_does_not_panic() {
        assert!(without_plugin_name(Vec::new()).is_empty());
        assert_eq!(
            without_plugin_name(argv(&["cargo-harbor"])),
            argv(&["cargo-harbor"])
        );
    }

    #[test]
    fn cargo_harbor_plan_parses() {
        let cli = parse(&["plan"]).expect("`cargo harbor plan` has to parse");
        assert!(matches!(cli.command, Command::Plan));
    }

    #[test]
    fn help_works_on_a_real_subcommand() {
        // The reason the plugin name is dropped rather than modelled as a clap subcommand:
        // with a `harbor` wrapper in the tree, `check --help` resolves the real subcommands
        // under `harbor` instead of at the top level, and stops working.
        let err = parse(&["check", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let text = err.to_string();
        assert!(
            text.contains("--plan"),
            "check's own flags are missing: {text}"
        );
    }

    #[test]
    fn global_flags_work_around_the_plugin_name() {
        // Both sides of it, because a workflow writes `--root .` before the subcommand while
        // a person tends to write it after.
        for args in [["--root", "/tmp/ws", "plan"], ["plan", "--root", "/tmp/ws"]] {
            let cli = parse(&args).unwrap();
            assert_eq!(cli.root, PathBuf::from("/tmp/ws"));
        }
    }

    #[test]
    fn the_prefix_defaults_to_no_filter() {
        // The safe direction, and the one a workspace with no shared name family needs:
        // a filter that is on by default silently drops crates from a release.
        let cli = parse(&["plan"]).unwrap();
        assert_eq!(cli.prefix, "");
    }

    #[test]
    fn a_locked_release_is_the_default() {
        let cli = parse(&["publish"]).unwrap();
        assert!(!cli.no_locked, "a release must not rewrite the lockfile");
    }

    #[test]
    fn no_order_flag_means_the_release_is_derived() {
        // Empty is "not asked for", which must not be read as "publish nothing".
        let cli = parse(&["publish"]).unwrap();
        assert!(cli.order.is_empty());
    }

    #[test]
    fn an_order_accepts_both_spellings() {
        // Repeated flags and a comma-separated list both have to work: the first is what a
        // person types, the second is what fits on one line of a workflow.
        let repeated = parse(&["publish", "--order", "acme-a", "--order", "acme-b"]).unwrap();
        let comma = parse(&["publish", "--order", "acme-a,acme-b"]).unwrap();
        assert_eq!(repeated.order, vec!["acme-a", "acme-b"]);
        assert_eq!(comma.order, repeated.order);
    }

    #[test]
    fn an_order_reaches_the_configuration() {
        // The wiring, which is what a flag with no effect would get wrong: `--order` has to
        // arrive at `explicit_order`, or the release silently derives the answer instead.
        let cli = parse(&["publish", "--order", "acme-tool"]).unwrap();
        assert_eq!(
            config_from(&cli).explicit_order,
            Some(vec!["acme-tool".to_string()])
        );

        // And with no flag it stays derived rather than becoming an empty release.
        let cli = parse(&["publish"]).unwrap();
        assert_eq!(config_from(&cli).explicit_order, None);
    }

    #[test]
    fn the_configuration_carries_the_global_flags() {
        let cli = parse(&["--prefix", "acme-", "--no-locked", "publish"]).unwrap();
        let config = config_from(&cli);
        assert_eq!(config.crate_prefix, "acme-");
        assert!(!config.locked);
    }

    #[test]
    fn the_publish_defaults_come_from_the_library() {
        // These used to be literals here *and* in `config.rs`, which is a default that
        // drifts. Parsing with no flags has to agree with `ReleaseConfig::default()`.
        let cli = parse(&["publish"]).unwrap();
        let defaults = ReleaseConfig::default();
        match &cli.command {
            Command::Publish {
                wait,
                attempts,
                dry_run,
            } => {
                assert_eq!(*wait, defaults.retry_delay.as_secs());
                assert_eq!(*attempts, defaults.max_attempts);
                assert!(!*dry_run);
            }
            other => panic!("expected publish, got {other:?}"),
        }
    }

    #[test]
    fn check_without_a_tag_is_allowed_so_it_can_be_run_on_a_branch() {
        let cli = parse(&["check"]).unwrap();
        match &cli.command {
            Command::Check { tag, plan } => {
                assert!(tag.is_none());
                assert!(!plan, "computing the plan is opt-in");
            }
            other => panic!("expected check, got {other:?}"),
        }
    }

    #[test]
    fn check_takes_a_tag_and_the_plan_flag() {
        let cli = parse(&["check", "v1.2.3", "--plan"]).unwrap();
        match &cli.command {
            Command::Check { tag, plan } => {
                assert_eq!(tag.as_deref(), Some("v1.2.3"));
                assert!(plan);
            }
            other => panic!("expected check, got {other:?}"),
        }
    }

    #[test]
    fn check_rejects_a_second_positional_argument() {
        // Silently ignoring it is how `check v1 v2` would look like it worked.
        assert!(parse(&["check", "v1.0.0", "v2.0.0"]).is_err());
    }

    #[test]
    fn root_and_prefix_are_global_flags() {
        // They have to be accepted before *and* after the subcommand, because a workflow
        // step and a person at a terminal tend to write them in different orders.
        for args in [["--root", "/tmp/ws", "plan"], ["plan", "--root", "/tmp/ws"]] {
            let cli = parse(&args).unwrap();
            assert_eq!(cli.root, PathBuf::from("/tmp/ws"));
        }
    }

    #[test]
    fn an_unknown_subcommand_is_rejected() {
        assert!(parse(&["publishh"]).is_err());
    }

    #[test]
    fn a_dry_run_is_opt_in_and_only_on_publish() {
        let cli = parse(&["publish", "--dry-run"]).unwrap();
        match &cli.command {
            Command::Publish { dry_run, .. } => assert!(dry_run),
            other => panic!("expected publish, got {other:?}"),
        }
        assert!(parse(&["plan", "--dry-run"]).is_err());
    }
}
