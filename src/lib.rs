//! Release orchestration for a Cargo workspace.
//!
//! # The problem this solves
//!
//! Publishing a workspace to a registry is a sequence with one correct shape and several
//! ways to get it silently wrong:
//!
//! - a crate cannot be published before everything it depends on, because `cargo publish`
//!   resolves each dependency against the *live* registry. So the order is a topological
//!   sort of the dependency graph, and a hand-maintained copy of it is a bug waiting to
//!   happen — the repository this crate was extracted from had exactly that bug, and it
//!   presented as a ten-minute retry loop and a misleading message about registry indexes;
//! - which crates ship is a property of each crate, not of a list beside a workflow;
//! - a partial release is normal, so a re-run must recognise what is already uploaded
//!   instead of failing on it;
//! - an index that has not caught up is a matter of time, whereas a bad token is not, and
//!   retrying both wastes ten minutes to produce the same failure.
//!
//! # Using it
//!
//! ```no_run
//! use harbor::{Release, ReleaseConfig};
//!
//! # fn main() -> Result<(), harbor::Error> {
//! // Every crate in the workspace is a candidate unless a prefix narrows it.
//! let release = Release::new(".").with_config(ReleaseConfig {
//!     crate_prefix: "acme-".into(),
//!     dry_run: true,
//!     ..ReleaseConfig::default()
//! });
//!
//! let plan = release.plan()?;
//! for krate in &plan.order {
//!     println!("{} {}", krate.name, krate.version);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! `no_run` because reading a workspace is exactly what it does, and the doctest would
//! otherwise try it against this crate's own directory — a workspace of one whose only
//! crate is `harbor`, and therefore nothing matching the prefix above.
//!
//! # What it deliberately does not do
//!
//! No version bumping, no changelog, no git tags, no binary packaging. Those are decisions
//! a project makes for itself. This crate answers one question — *what publishes, in what
//! order, and did it work* — and answers it well enough to run unattended.
//!
//! # Testing a release
//!
//! [`Publisher::with_runner`] takes a [`CommandRunner`], so the whole publishing policy can
//! be exercised without a registry: the order, the re-run behaviour, the retry budget, and
//! the exact commands issued are all observable from a test. See `publish::tests`.

#![warn(missing_docs)]
#![warn(missing_debug_implementations)]
#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod gha;
pub mod graph;
pub mod manifest;
pub mod preflight;
pub mod publish;
pub mod runner;
pub mod workspace;

pub use crate::config::ReleaseConfig;
pub use crate::error::Error;
pub use crate::error::Result;
pub use crate::graph::Crate;
pub use crate::graph::Dep;
pub use crate::graph::DepKind;
pub use crate::graph::Graph;
pub use crate::graph::Plan;
pub use crate::publish::Publisher;
pub use crate::publish::ReleaseReport;
pub use crate::runner::CommandRunner;
pub use crate::runner::Outcome;
pub use crate::runner::SystemRunner;

use std::path::Path;
use std::path::PathBuf;

/// A release of one workspace.
///
/// Cheap to build and carries no state beyond its root and its configuration, so it can be
/// created per call rather than threaded through a program.
#[derive(Debug, Clone)]
pub struct Release {
    root: PathBuf,
    config: ReleaseConfig,
}

impl Release {
    /// A release of the workspace rooted at `root`.
    ///
    /// Nothing is read until [`Release::plan`] is called, so a bad path fails there rather
    /// than here — with a message naming the file that could not be read.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            config: ReleaseConfig::default(),
        }
    }

    /// The same release with a different configuration.
    pub fn with_config(mut self, config: ReleaseConfig) -> Self {
        self.config = config;
        self
    }

    /// The configuration in use.
    pub fn config(&self) -> &ReleaseConfig {
        &self.config
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Computes what would be published, in what order.
    ///
    /// Never touches the network. Every guard against an unpublishable workspace — a
    /// dependency cycle, a published crate depending on an excluded one, a member pattern
    /// that matches nothing — is applied here, so an error from `plan` means nothing would
    /// have been uploaded.
    pub fn plan(&self) -> Result<Plan> {
        workspace::plan(&self.root, &self.config)
    }

    /// Publishes every crate in dependency order.
    pub fn publish(&self) -> Result<ReleaseReport> {
        let plan = self.plan()?;
        // A dry run is reported as what it is rather than as a list of uploads, so a caller
        // cannot mistake one for the other.
        if self.config.dry_run {
            gha::notice("dry run: nothing will be uploaded");
        }
        Publisher::run(&plan, &self.config)
    }

    /// Checks that `tag` names this workspace's version.
    pub fn check_tag(&self, tag: &str) -> Result<String> {
        preflight::check_tag(&self.root, tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace on disk, exercised through the public API only.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"0.0.5\"\n",
        )
        .unwrap();
        for (name, deps) in [
            ("acme-core", ""),
            ("acme-map", "acme-core.workspace = true"),
        ] {
            let path = dir.path().join("crates").join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{name}\"\nversion.workspace = true\n\n[dependencies]\n{deps}\n"
                ),
            )
            .unwrap();
        }
        dir
    }

    fn release(dir: &tempfile::TempDir) -> Release {
        Release::new(dir.path()).with_config(ReleaseConfig {
            crate_prefix: "acme-".to_string(),
            ..ReleaseConfig::default()
        })
    }

    #[test]
    fn a_plan_lists_crates_in_dependency_order() {
        let dir = workspace();
        let plan = release(&dir).plan().unwrap();
        assert_eq!(plan.names(), vec!["acme-core", "acme-map"]);
        assert!(!plan.is_empty());
        assert_eq!(plan.len(), 2);
        assert!(plan.excluded().is_empty(), "both crates are publishable");
    }

    #[test]
    fn a_dry_run_publishes_nothing_and_says_so() {
        let dir = workspace();
        let release = Release::new(dir.path()).with_config(ReleaseConfig {
            crate_prefix: "acme-".to_string(),
            dry_run: true,
            ..ReleaseConfig::default()
        });
        let report = release.publish().unwrap();
        assert!(report.dry_run);
        assert_eq!(report.total(), 2);
    }

    #[test]
    fn the_tag_check_is_reachable_from_the_release() {
        let dir = workspace();
        assert_eq!(release(&dir).check_tag("v0.0.5").unwrap(), "0.0.5");
        assert!(release(&dir).check_tag("v0.0.4").is_err());
    }

    #[test]
    fn config_and_root_are_readable_back() {
        let dir = workspace();
        let release = release(&dir);
        assert_eq!(release.config().crate_prefix, "acme-");
        assert_eq!(release.root(), dir.path());
    }

    #[test]
    fn turning_off_the_publish_flag_puts_an_excluded_crate_back_in_the_plan() {
        // End to end, through the public API, for the field that used to be read by nothing.
        let dir = workspace();
        let cli = dir.path().join("crates").join("acme-cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(
            cli.join("Cargo.toml"),
            "[package]\nname = \"acme-cli\"\nversion.workspace = true\npublish = false\n",
        )
        .unwrap();

        // Honoured: the crate is left out, and reported as left out.
        let honouring = release(&dir).plan().unwrap();
        assert_eq!(honouring.names(), vec!["acme-core", "acme-map"]);
        assert_eq!(honouring.excluded().len(), 1);
        assert_eq!(honouring.excluded()[0].name, "acme-cli");

        // Ignored: the same workspace publishes it.
        let ignoring = Release::new(dir.path())
            .with_config(ReleaseConfig {
                crate_prefix: "acme-".to_string(),
                respect_publish_flag: false,
                ..ReleaseConfig::default()
            })
            .plan()
            .unwrap();
        assert!(
            ignoring.names().contains(&"acme-cli"),
            "{:?}",
            ignoring.names()
        );
        assert!(ignoring.excluded().is_empty());
    }
}
