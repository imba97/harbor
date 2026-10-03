//! Running `cargo`, behind a trait.
//!
//! The trait is the seam that makes a release testable. Everything a release decides — the
//! order, what to do when a crate is already uploaded, how long to wait for an index, when
//! to stop — is policy, and policy is worth testing. Whether a process actually spawns is
//! not: that is `std`'s job and it is exercised once, by running a release for real.
//!
//! So the tests drive a recording fake and assert on the *sequence of commands*, which is
//! the part that was wrong last time (a crate was published before its dependency).

use std::path::Path;

use crate::error::Error;
use crate::error::Result;

/// The output of a finished process.
#[derive(Debug, Clone)]
pub struct Output {
    /// Whether the process exited zero.
    pub success: bool,
    /// Everything it printed, both streams, in arrival order.
    pub text: String,
}

/// Something that can run `cargo`.
pub trait CommandRunner {
    /// Runs `program args...` with `dir` as the working directory.
    ///
    /// A process that cannot be *started* is an error. A process that starts and fails is
    /// [`Output::success`] being false, because a failing `cargo publish` is a result this
    /// crate has to classify, not a transport problem.
    fn run(&self, program: &str, args: &[String], dir: &Path) -> Result<Output>;
}

/// Runs real processes.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[String], dir: &Path) -> Result<Output> {
        let output = std::process::Command::new(program)
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(|e| Error::Process {
                command: render(program, args),
                message: e.to_string(),
            })?;

        // Both streams, concatenated. cargo explains a publish failure across both, and
        // losing one of them is how a real reason turns into a guess.
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));

        Ok(Output {
            success: output.status.success(),
            text,
        })
    }
}

/// Renders a command line the way a user would type it.
pub(crate) fn render(program: &str, args: &[String]) -> String {
    let mut line = program.to_string();
    for arg in args {
        line.push(' ');
        // Quote only when it would otherwise be misread, so the common case stays readable.
        if arg.contains(' ') {
            line.push('"');
            line.push_str(arg);
            line.push('"');
        } else {
            line.push_str(arg);
        }
    }
    line
}

/// What `cargo publish` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The crate was uploaded.
    Published,
    /// The registry already has this exact name and version, which crates.io never allows
    /// to be replaced. A re-run after a partial release lands here, and it is not a
    /// failure: the crate is out, so the release moves on.
    AlreadyPublished,
    /// A dependency is not resolvable yet, which is crates.io's index catching up rather
    /// than a problem with this crate. The only outcome worth waiting on.
    IndexNotCaughtUp,
    /// Anything else. Reported verbatim and stops the release: a retry would only waste
    /// ten minutes before failing the same way.
    Failed,
}

impl Outcome {
    /// Classifies the output of a finished `cargo publish`.
    pub fn classify(output: &Output) -> Self {
        if output.success {
            return Self::Published;
        }
        let text = output.text.to_ascii_lowercase();

        // Order matters. "failed to select a version" and "already exists" cannot both
        // appear in a real failure, but if a future cargo ever does emit both, the
        // recoverable reading is the safer one to take.
        if text.contains("already uploaded") || text.contains("already exists") {
            return Self::AlreadyPublished;
        }
        if text.contains("failed to select a version") {
            return Self::IndexNotCaughtUp;
        }
        Self::Failed
    }

    /// Whether the release can carry on to the next crate without retrying.
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Published | Self::AlreadyPublished)
    }
}

/// The `cargo publish` invocation for one crate.
///
/// The version deliberately does not appear: cargo takes it from the manifest, and passing
/// it would be inventing a flag. `--package` is what selects the crate.
pub(crate) fn publish_args(crate_name: &str, locked: bool) -> Vec<String> {
    let mut args = vec![
        "publish".to_string(),
        "--package".to_string(),
        crate_name.to_string(),
    ];
    if locked {
        args.push("--locked".to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(success: bool, text: &str) -> Output {
        Output {
            success,
            text: text.to_string(),
        }
    }

    #[test]
    fn a_zero_exit_is_a_publish() {
        assert_eq!(
            Outcome::classify(&out(true, "Uploading acme-core v0.0.5")),
            Outcome::Published
        );
    }

    #[test]
    fn the_real_already_exists_message_is_recognised() {
        // Recorded from crates.io: this is what a re-run of a partial release sees.
        let text = "error: crate version `0.0.5` is already uploaded\n\
                    You cannot replace a crate version once it is uploaded.";
        assert_eq!(
            Outcome::classify(&out(false, text)),
            Outcome::AlreadyPublished
        );
    }

    #[test]
    fn the_real_index_propagation_message_is_recognised() {
        // Recorded from an actual failed release, and the reason the shell this replaced
        // retried at all.
        let text = "error: failed to prepare local package for uploading\n\
                    \n\
                    Caused by:\n\
                      failed to select a version for the requirement `acme-map = \"^0.0.5\"`\n\
                      candidate versions found which didn't match: 0.0.2\n\
                      location searched: crates.io index";
        assert_eq!(
            Outcome::classify(&out(false, text)),
            Outcome::IndexNotCaughtUp
        );
    }

    #[test]
    fn classification_is_case_insensitive() {
        let text = "Crate Version `0.0.5` Is Already Uploaded";
        assert_eq!(
            Outcome::classify(&out(false, text)),
            Outcome::AlreadyPublished
        );
    }

    #[test]
    fn an_unrecognised_failure_is_not_retried() {
        // The important negative: a bad token or a compile error must stop the release
        // rather than burn ten minutes of retries.
        let text = "error: failed to get a token\nCaused by: no token found";
        assert_eq!(Outcome::classify(&out(false, text)), Outcome::Failed);
        assert!(!Outcome::Failed.is_done());
    }

    #[test]
    fn done_covers_exactly_the_two_recoverable_outcomes() {
        assert!(Outcome::Published.is_done());
        assert!(Outcome::AlreadyPublished.is_done());
        assert!(!Outcome::IndexNotCaughtUp.is_done());
        assert!(!Outcome::Failed.is_done());
    }

    #[test]
    fn locked_is_only_passed_when_asked_for() {
        let args = publish_args("acme-core", true);
        assert!(args.contains(&"--locked".to_string()));
        assert!(args.contains(&"acme-core".to_string()));
        assert!(!publish_args("acme-core", false).contains(&"--locked".to_string()));
    }

    #[test]
    fn a_rendered_command_quotes_only_what_needs_it() {
        let args = vec!["publish".to_string(), "a b".to_string(), "c".to_string()];
        assert_eq!(render("cargo", &args), "cargo publish \"a b\" c");
    }
}
