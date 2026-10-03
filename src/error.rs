//! The failure type.
//!
//! One enum rather than a boxed error, because a caller of this library — a CI step, most
//! likely — has to tell a *configuration* problem (nothing was published, and nothing
//! should be) from an *operational* one (some crates are out, and a re-run is needed).
//! Those two want opposite responses, so collapsing them into one string would take away
//! the only decision the caller has to make.

use std::path::PathBuf;

/// Anything that stops a release.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A manifest could not be read or understood.
    #[error("{path}: {message}")]
    Manifest {
        /// The file being read.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },

    /// The workspace itself is not in a state that can be released.
    #[error("{0}")]
    Workspace(String),

    /// A `cargo` invocation could not be started, or died without reporting why.
    #[error("running `{command}`: {message}")]
    Process {
        /// The command line that failed, as a human would type it.
        command: String,
        /// What the operating system said.
        message: String,
    },

    /// `cargo` ran and reported a failure this crate does not know how to recover from.
    #[error("{}: cargo publish failed\n{}", crate_name, output.join("\n"))]
    Publish {
        /// The crate that failed.
        crate_name: String,
        /// What cargo printed, as lines.
        ///
        /// Kept as lines rather than one blob so a caller can show just the first few, put
        /// them in a log group, or store them whole — all of which mean something different
        /// for a multi-line message. Nothing here summarises cargo's words away: the real
        /// reason is the only thing worth reporting.
        output: Vec<String>,
    },

    /// The registry never acknowledged a dependency, after waiting.
    #[error(
        "{crate_name} still cannot resolve its dependencies after {waited_secs}s; \
         the registry index has not caught up and the release stopped here. \
         Re-running it continues from this crate."
    )]
    IndexNotCaughtUp {
        /// The crate that was being published.
        crate_name: String,
        /// How long was spent waiting, in seconds.
        waited_secs: u64,
    },

    /// A preflight check failed.
    #[error("{0}")]
    Preflight(String),
}

impl Error {
    /// Builds a [`Error::Manifest`].
    pub(crate) fn manifest(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Manifest {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// The result of anything in this crate.
pub type Result<T> = std::result::Result<T, Error>;
