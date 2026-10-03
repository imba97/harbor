//! Publishing a plan, in order, tolerating the registry's timing.
//!
//! The rules here are the ones a hand-written loop kept getting subtly wrong:
//!
//! - the plan's order is followed exactly, and it was computed from the graph rather than
//!   typed out, so a crate cannot be published before its dependency;
//! - a crate the registry already holds is not an error, because crates.io never allows a
//!   re-upload and a re-run after a partial release is the normal way to finish one;
//! - an index that has not caught up is retried, because that is a matter of time;
//! - anything else stops immediately, because a retry would spend ten minutes failing the
//!   same way.

use std::thread::sleep;

use crate::config::ReleaseConfig;
use crate::error::Error;
use crate::error::Result;
use crate::graph::Plan;
use crate::runner::publish_args;
use crate::runner::CommandRunner;
use crate::runner::Outcome;
use crate::runner::Output;
use crate::runner::SystemRunner;

/// What a release did, for a caller that wants to report on it.
#[derive(Debug, Default, Clone)]
pub struct ReleaseReport {
    /// Crates uploaded by this run.
    pub published: Vec<String>,
    /// Crates the registry already had, which is a re-run finishing an earlier release.
    pub already_present: Vec<String>,
    /// Whether anything was actually uploaded.
    pub dry_run: bool,
}

impl ReleaseReport {
    /// How many crates are now on the registry because of this run, one way or another.
    pub fn total(&self) -> usize {
        self.published.len() + self.already_present.len()
    }
}

/// Publishes plans.
#[derive(Debug, Clone, Copy, Default)]
pub struct Publisher;

impl Publisher {
    /// Publishes every crate in `plan`, in order, using real `cargo` processes.
    pub fn run(plan: &Plan, config: &ReleaseConfig) -> Result<ReleaseReport> {
        Self::with_runner(plan, config, &SystemRunner)
    }

    /// Publishes using `runner`, which is what makes a release testable without a registry.
    pub fn with_runner(
        plan: &Plan,
        config: &ReleaseConfig,
        runner: &dyn CommandRunner,
    ) -> Result<ReleaseReport> {
        let mut report = ReleaseReport {
            dry_run: config.dry_run,
            ..ReleaseReport::default()
        };

        for krate in &plan.order {
            if config.dry_run {
                // Checked before anything is spawned, not inside `publish_one`: a dry run
                // must not reach the registry even once.
                crate::gha::group(&format!("{} {}", krate.name, krate.version));
                println!("would publish {} {}", krate.name, krate.version);
                crate::gha::end_group();
                report.published.push(krate.name.clone());
                continue;
            }

            // The group is closed on the way out whatever happened, including the error
            // path. It used to be closed after a `?`, which meant a failed crate left
            // GitHub Actions inside an unclosed `::group::` — every later line of the log
            // nested under it, and the next `::endgroup::` closed the wrong one.
            crate::gha::group(&format!("{} {}", krate.name, krate.version));
            let outcome = publish_one(runner, plan, config, &krate.name, &krate.version);
            crate::gha::end_group();
            let newly_published = outcome?;

            if newly_published {
                println!("published {} {}", krate.name, krate.version);
                report.published.push(krate.name.clone());
            } else {
                println!(
                    "{} {} is already on the registry, moving on",
                    krate.name, krate.version
                );
                report.already_present.push(krate.name.clone());
            }
        }

        Ok(report)
    }
}

/// Publishes one crate, retrying only what is worth retrying.
///
/// Returns whether this run uploaded it. A crate the registry already holds is `false`,
/// which is a success: crates.io never allows a re-upload, so a re-run finishing a partial
/// release is the normal case rather than an error.
///
/// The return type is deliberately narrow. It used to hand back a four-variant
/// [`Outcome`], of which two were impossible by construction and had to be handled with
/// `unreachable!()` at the call site — a runtime panic standing in for a type that could
/// simply have said which two outcomes it produces.
fn publish_one(
    runner: &dyn CommandRunner,
    plan: &Plan,
    config: &ReleaseConfig,
    name: &str,
    version: &str,
) -> Result<bool> {
    let args = publish_args(name, config.locked);
    let attempts = config.attempts();

    for attempt in 1..=attempts {
        let output = runner.run("cargo", &args, &plan.root)?;

        match Outcome::classify(&output) {
            Outcome::Published => return Ok(true),
            Outcome::AlreadyPublished => return Ok(false),
            Outcome::Failed => {
                return Err(Error::Publish {
                    crate_name: name.to_string(),
                    output: output.text.lines().map(str::to_string).collect::<Vec<_>>(),
                });
            }
            Outcome::IndexNotCaughtUp => {
                if attempt == attempts {
                    return Err(Error::IndexNotCaughtUp {
                        crate_name: name.to_string(),
                        waited_secs: config.total_wait_secs(),
                    });
                }
                println!(
                    "{}: a dependency is not indexed yet, retrying ({attempt}/{attempts})",
                    name
                );
                print_output(&output);
                crate::gha::notice(&format!(
                    "{name} {version}: waiting {}s for the registry index",
                    config.retry_delay.as_secs()
                ));
                sleep(config.retry_delay);
            }
        }
    }

    // Unreachable: the loop returns on the final attempt. Kept as an error rather than a
    // panic so a future change to the loop cannot turn a release into a crash.
    Err(Error::IndexNotCaughtUp {
        crate_name: name.to_string(),
        waited_secs: config.total_wait_secs(),
    })
}

/// Echoes what cargo said, so a retry does not hide the reason for it.
fn print_output(output: &Output) {
    let text = output.text.trim_end();
    if !text.is_empty() {
        println!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Crate;
    use crate::runner::render;
    use crate::runner::Output;
    use std::cell::RefCell;
    use std::path::Path;
    use std::path::PathBuf;
    use std::time::Duration;

    /// A runner that replays canned output and records every command it was asked to run.
    ///
    /// Responses are consumed in order; once the queue is empty the last one repeats, which
    /// is what expresses "this keeps happening" without listing it out.
    struct Fake {
        responses: RefCell<Vec<(bool, String)>>,
        calls: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(responses: &[(&str, bool)]) -> Self {
            Self {
                responses: RefCell::new(
                    responses
                        .iter()
                        .map(|(text, ok)| (*ok, (*text).to_string()))
                        .collect(),
                ),
                calls: RefCell::new(Vec::new()),
            }
        }

        /// Always answers the same thing.
        fn always(text: &str, ok: bool) -> Self {
            Self::new(&[(text, ok)])
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &str, args: &[String], _dir: &Path) -> Result<Output> {
            self.calls.borrow_mut().push(render(program, args));
            let mut queue = self.responses.borrow_mut();
            let (success, text) = if queue.len() > 1 {
                queue.remove(0)
            } else {
                queue
                    .first()
                    .cloned()
                    .unwrap_or_else(|| (true, String::new()))
            };
            Ok(Output { success, text })
        }
    }

    /// A plan over the named crates, all publishable and with no dependencies between them.
    fn plan_of(names: &[&str], config: &ReleaseConfig) -> Plan {
        let mut graph = crate::graph::Graph::new();
        for name in names {
            graph.insert(Crate {
                name: (*name).to_string(),
                version: "0.4.0".to_string(),
                publish: true,
            });
        }
        let order = graph.publish_order(config).unwrap();
        Plan::new(PathBuf::from("."), graph, order, config.clone())
    }

    fn config() -> ReleaseConfig {
        ReleaseConfig {
            // No prefix filter needed here: these plans are built directly rather than read
            // from a workspace, which is the only place the prefix applies.
            crate_prefix: String::new(),
            retry_delay: Duration::from_millis(0),
            max_attempts: 3,
            ..ReleaseConfig::default()
        }
    }

    /// A plan over the named crates, using [`config`].
    fn planned(names: &[&str]) -> Plan {
        plan_of(names, &config())
    }

    /// The invariant a failed release used to break: a `::group::` with no `::endgroup::`.
    ///
    /// Checked through `gha`'s counter rather than by capturing stdout, because the
    /// invariant holds whether or not commands are being emitted for a log — and because a
    /// test that redirects the process's own stdout is a test that can swallow the
    /// harness's.
    ///
    /// The counter is thread-local, and the test harness reuses threads, so this records the
    /// value *before* the work as well as after: what is being asserted is that the release
    /// left the count where it found it, not that it happens to be zero on a thread some
    /// other test has already used.
    fn groups_open() -> usize {
        crate::gha::open_groups()
    }

    /// Runs `body` and asserts the open-group count is unchanged, returning what it produced.
    ///
    /// The count is read once into `after` rather than called twice inside the assertion:
    /// the two calls in `assert_eq!` are evaluated in an order the macro does not promise,
    /// and reading a counter that `body` has just changed twice is how a correct assertion
    /// reports the wrong values.
    fn balanced<T>(body: impl FnOnce() -> T) -> T {
        let before = groups_open();
        let produced = body();
        let after = groups_open();
        assert_eq!(
            after,
            before,
            "the release left {} group(s) open; every later log line would nest under them",
            after.abs_diff(before)
        );
        produced
    }

    #[test]
    fn crates_are_published_in_the_plans_order() {
        let plan = planned(&["acme-a", "acme-b", "acme-c"]);
        let fake = Fake::always("uploaded", true);
        let config = config();
        let report = balanced(|| Publisher::with_runner(&plan, &config, &fake).unwrap());

        assert_eq!(report.published, vec!["acme-a", "acme-b", "acme-c"]);
        assert_eq!(
            fake.calls(),
            vec![
                "cargo publish --package acme-a --locked",
                "cargo publish --package acme-b --locked",
                "cargo publish --package acme-c --locked",
            ]
        );
    }

    #[test]
    fn a_failure_does_not_leave_a_group_open() {
        // The regression: `end_group` used to sit after a `?`, so the error path skipped it
        // and every later line of the CI log nested inside the failed crate's group.
        let plan = planned(&["acme-a", "acme-b"]);
        let fake = Fake::always("error: no token found", false);
        let config = config();
        let result = balanced(|| Publisher::with_runner(&plan, &config, &fake));
        assert!(result.is_err());
    }

    #[test]
    fn an_already_uploaded_crate_does_not_stop_the_release() {
        // The partial-release re-run: the first crate is already out, so the run continues.
        let plan = planned(&["acme-a", "acme-b"]);
        let fake = Fake::new(&[
            ("error: crate version `0.0.5` is already uploaded", false),
            ("uploaded", true),
        ]);
        let report = Publisher::with_runner(&plan, &config(), &fake).unwrap();

        assert_eq!(report.already_present, vec!["acme-a"]);
        assert_eq!(report.published, vec!["acme-b"]);
        assert_eq!(report.total(), 2);
    }

    #[test]
    fn an_unindexed_dependency_is_retried_and_then_succeeds() {
        let plan = planned(&["acme-a"]);
        let fake = Fake::new(&[
            ("failed to select a version for the requirement `x`", false),
            ("failed to select a version for the requirement `x`", false),
            ("uploaded", true),
        ]);
        let report = Publisher::with_runner(&plan, &config(), &fake).unwrap();
        assert_eq!(report.published, vec!["acme-a"]);
    }

    #[test]
    fn a_persistent_index_delay_fails_with_the_crate_named() {
        let plan = planned(&["acme-a"]);
        let fake = Fake::always("failed to select a version", false);
        let err = Publisher::with_runner(&plan, &config(), &fake).unwrap_err();
        match err {
            Error::IndexNotCaughtUp { crate_name, .. } => assert_eq!(crate_name, "acme-a"),
            other => panic!("expected an index error, got {other:?}"),
        }
    }

    #[test]
    fn a_real_failure_stops_at_once_without_retrying() {
        // A bad token, a compile error: retrying would waste the budget and fail the same
        // way, so the command is issued exactly once.
        let plan = planned(&["acme-a", "acme-b"]);
        let fake = Fake::always("error: no token found", false);
        let err = Publisher::with_runner(&plan, &config(), &fake).unwrap_err();

        match err {
            Error::Publish { crate_name, output } => {
                assert_eq!(crate_name, "acme-a");
                assert!(output.iter().any(|line| line.contains("no token found")));
            }
            other => panic!("expected a publish error, got {other:?}"),
        }
        assert_eq!(
            fake.calls().len(),
            1,
            "must not retry an unrecoverable failure"
        );
    }

    #[test]
    fn a_failure_carries_cargos_own_words_rather_than_a_summary() {
        let plan = planned(&["acme-a"]);
        let fake = Fake::always("error: failed to verify package tarball", false);
        let err = Publisher::with_runner(&plan, &config(), &fake).unwrap_err();
        assert!(
            err.to_string().contains("failed to verify package tarball"),
            "{err}"
        );
    }

    #[test]
    fn a_dry_run_runs_nothing() {
        let plan = planned(&["acme-a", "acme-b"]);
        let fake = Fake::always("should not be called", true);
        let config = ReleaseConfig {
            dry_run: true,
            ..config()
        };
        let report = Publisher::with_runner(&plan, &config, &fake).unwrap();

        assert!(report.dry_run);
        assert_eq!(report.published.len(), 2);
        assert!(
            fake.calls().is_empty(),
            "a dry run must not touch the registry"
        );
    }

    #[test]
    fn an_empty_plan_publishes_nothing_and_is_not_an_error() {
        let plan = planned(&[]);
        let fake = Fake::always("", true);
        let report = Publisher::with_runner(&plan, &config(), &fake).unwrap();
        assert_eq!(report.total(), 0);
        assert!(fake.calls().is_empty());
    }
}
