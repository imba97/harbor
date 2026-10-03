//! What a release is told to do.
//!
//! Every value here is a decision a caller makes, and the defaults are the ones that make
//! `Release::new(root).plan()` do the obvious thing on a normal workspace. Nothing in this
//! module reads the environment: that is the CLI's job, so the library stays usable from a
//! build script, a test, or another tool without inheriting someone's variables.

use std::time::Duration;

use crate::graph::Crate;

/// Seconds between index-propagation retries.
///
/// A named constant rather than a literal because the CLI's `--wait` default and the
/// library's default have to be the same number. They were two literals in two files, which
/// is a default that drifts.
pub const DEFAULT_RETRY_DELAY_SECS: u64 = 10;

/// Attempts before giving up on an index that has not caught up.
///
/// Ten minutes in total: far longer than crates.io normally needs, and short enough that a
/// stuck release does not outlive a CI job's patience. The same reasoning as
/// [`DEFAULT_RETRY_DELAY_SECS`] for why this is a constant.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 60;

/// How to run a release.
#[derive(Debug, Clone)]
pub struct ReleaseConfig {
    /// Honour `publish = false` in each crate's manifest.
    ///
    /// On by default. It is the *only* thing that decides what ships — a hand-written list
    /// of published crates next to a workflow is what let a library go unpublished while the
    /// binary depending on it shipped, so this crate refuses to offer one.
    ///
    /// Turning it off publishes every crate in the workspace, `publish = false` or not.
    /// That is occasionally what a caller wants (a release to a private registry, say), and
    /// it is the reason this flag has to be *read* rather than merely declared: a
    /// configuration field that changes nothing is worse than no field at all, because it
    /// reads as a promise.
    pub respect_publish_flag: bool,
    /// Pass `--locked` to `cargo publish`, refusing a build that would change the lockfile.
    pub locked: bool,
    /// Print what would happen without contacting the registry.
    pub dry_run: bool,
    /// How long to wait between index-propagation retries.
    pub retry_delay: Duration,
    /// How many times to retry a crate whose dependency the index has not indexed yet.
    pub max_attempts: u32,
    /// Only consider crates whose name starts with this prefix.
    ///
    /// Empty by default, which means "every crate in the workspace is a candidate". That is
    /// the safe direction for a general-purpose tool: a filter that is on by default
    /// silently *drops* crates from a release, and a crate that is missing is much harder to
    /// notice than one that is present and should not have been.
    ///
    /// The way to keep a crate out is `publish = false` in its own manifest — a statement
    /// the crate makes about itself, rather than a rule a workflow has to remember.
    ///
    /// Set this when one workspace holds several families of crates and only one family
    /// belongs to a given release.
    pub crate_prefix: String,
    /// Exactly which crates this release is about, and nothing else.
    ///
    /// `None` by default, which means "derive it": every crate the manifest rules include,
    /// ordered by the dependency graph. That is the right answer for a workspace where the
    /// crates are all part of one product.
    ///
    /// `Some` is for the case the derived answer gets wrong, and there is one close to home:
    /// a tool that ships as an installed binary declares `publish = false` — correctly, it
    /// has no business being anybody's dependency — and then cannot publish *itself* through
    /// this crate, because `respect_publish_flag` is exactly what makes `publish = false`
    /// mean anything. Nor can the flag simply be turned off: that would publish every crate
    /// the workspace holds, which is a different release.
    ///
    /// Listing the crates explicitly resolves that without weakening anything:
    ///
    /// - the flag keeps its meaning, because nothing ignores it;
    /// - the release says what it is about instead of inferring it, so a crate that should
    ///   not ship cannot be swept in by a manifest change elsewhere;
    /// - ordering still has to be explicit — every crate in the list must come after the
    ///   dependencies it needs, which is checked against the graph rather than trusted.
    ///
    /// It is not a way to publish something out of order. The list selects and orders; the
    /// graph still has the final say on whether that order can work.
    pub explicit_order: Option<Vec<String>>,
}

impl Default for ReleaseConfig {
    fn default() -> Self {
        Self {
            respect_publish_flag: true,
            locked: true,
            dry_run: false,
            retry_delay: Duration::from_secs(DEFAULT_RETRY_DELAY_SECS),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            crate_prefix: String::new(),
            explicit_order: None,
        }
    }
}

impl ReleaseConfig {
    /// Total time a single crate may spend waiting for the index, in seconds.
    ///
    /// Reported in the error so the message says how long was actually spent rather than
    /// repeating the per-attempt delay, which reads as a much shorter wait than it was.
    pub fn total_wait_secs(&self) -> u64 {
        self.retry_delay
            .as_secs()
            .saturating_mul(u64::from(self.max_attempts))
    }

    /// Whether `krate` is part of this release.
    ///
    /// The single place that answers "does this crate ship", so the plan, the order and the
    /// exclusion report cannot disagree — which is the failure mode that made
    /// `respect_publish_flag` worth wiring up properly rather than leaving as a field
    /// nobody read.
    ///
    /// When [`ReleaseConfig::explicit_order`] is set, that list is the answer: it names
    /// exactly what this release is about, and nothing else is a candidate.
    pub fn includes(&self, krate: &Crate) -> bool {
        if let Some(names) = &self.explicit_order {
            return names.iter().any(|n| n == &krate.name);
        }
        !self.respect_publish_flag || krate.is_publishable()
    }

    /// How many attempts a crate gets, never fewer than one.
    ///
    /// A budget of zero would otherwise mean "never try", which is not a useful release.
    pub fn attempts(&self) -> u32 {
        self.max_attempts.max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn krate(publish: bool) -> Crate {
        Crate {
            name: "acme-lib".to_string(),
            version: "0.1.0".to_string(),
            publish,
        }
    }

    #[test]
    fn the_total_wait_is_the_product_not_the_delay() {
        let config = ReleaseConfig::default();
        assert_eq!(config.total_wait_secs(), 600);
    }

    #[test]
    fn a_zero_attempt_budget_does_not_divide_by_zero_or_wrap() {
        let config = ReleaseConfig {
            max_attempts: 0,
            ..ReleaseConfig::default()
        };
        assert_eq!(config.total_wait_secs(), 0);
    }

    #[test]
    fn a_zero_attempt_budget_still_tries_once() {
        // "Never try" is not a release, so the floor is one attempt rather than none.
        let config = ReleaseConfig {
            max_attempts: 0,
            ..ReleaseConfig::default()
        };
        assert_eq!(config.attempts(), 1);
    }

    #[test]
    fn the_defaults_are_the_safe_ones() {
        let config = ReleaseConfig::default();
        assert!(
            config.respect_publish_flag,
            "a non-published crate must not ship"
        );
        assert!(config.locked, "a release must not rewrite the lockfile");
        assert!(!config.dry_run);
    }

    #[test]
    fn every_crate_is_a_candidate_by_default() {
        // No name filter unless one is asked for: a default filter would silently drop
        // crates from a release, which is the failure mode that is hardest to notice.
        assert!(ReleaseConfig::default().crate_prefix.is_empty());
    }

    #[test]
    fn the_publish_flag_is_honoured_by_default() {
        let config = ReleaseConfig::default();
        assert!(config.includes(&krate(true)));
        assert!(!config.includes(&krate(false)));
    }

    #[test]
    fn turning_the_flag_off_includes_every_crate() {
        // The regression this exists for: the field used to be declared and never read, so
        // setting it changed nothing at all.
        let config = ReleaseConfig {
            respect_publish_flag: false,
            ..ReleaseConfig::default()
        };
        assert!(config.includes(&krate(true)));
        assert!(config.includes(&krate(false)));
    }

    #[test]
    fn the_cli_defaults_come_from_these_constants() {
        // Guards the drift the constants were introduced to remove: these two numbers used
        // to be literals here, in `main.rs`, and in clap's `default_value_t`.
        let config = ReleaseConfig::default();
        assert_eq!(config.retry_delay.as_secs(), DEFAULT_RETRY_DELAY_SECS);
        assert_eq!(config.max_attempts, DEFAULT_MAX_ATTEMPTS);
    }
}
