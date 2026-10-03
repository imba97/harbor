//! The dependency graph, and the order a release has to walk it in.
//!
//! # Why the order is computed and not configured
//!
//! `cargo publish` resolves each dependency against the *live* registry, so a crate cannot
//! be published until everything it depends on is already there. That makes the order a
//! topological sort of this graph: it has one correct answer up to ties, and a
//! hand-maintained copy of it is therefore guaranteed to rot.
//!
//! It did. In the project this crate was extracted from, the order was a string in a
//! workflow file, and at some point one crate was listed before another that it depended
//! on. The publish loop retried the impossible dependency for ten minutes and then reported
//! a problem with the registry index — nothing in that failure pointed at the string.
//!
//! So: the order is derived, ties are broken by name so two runs agree, and the ways the
//! graph can be unpublishable are checked before anything is uploaded rather than
//! discovered half-way through.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::config::ReleaseConfig;
use crate::error::Error;
use crate::error::Result;

/// Which table a dependency was declared in.
///
/// Only the distinction the release needs: a normal dependency and a build dependency both
/// have to be on the registry before the crate can be published, whereas a dev-dependency
/// is not part of what a published crate asks for and imposes no ordering at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DepKind {
    /// `[dependencies]`.
    Normal,
    /// `[build-dependencies]`.
    Build,
}

/// One crate in the workspace.
#[derive(Debug, Clone)]
pub struct Crate {
    /// The package name, as it appears on the registry.
    pub name: String,
    /// The version, already resolved through `version.workspace = true` if it used that.
    pub version: String,
    /// Whether the manifest left `publish` unset or set it to something other than false.
    ///
    /// Read from the crate's own manifest, never from a list beside a workflow: which
    /// crates ship is a property of each crate. Whether it is *honoured* is
    /// [`ReleaseConfig::respect_publish_flag`]'s decision — see [`ReleaseConfig::includes`].
    pub publish: bool,
}

impl Crate {
    /// Whether this crate is a candidate for publishing.
    pub fn is_publishable(&self) -> bool {
        self.publish
    }
}

/// A dependency on another crate in the same workspace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dep {
    /// The dependency's package name.
    pub name: String,
    /// Which table declared it.
    pub kind: DepKind,
}

/// The workspace's crates and the edges between them.
#[derive(Debug, Default, Clone)]
pub struct Graph {
    crates: BTreeMap<String, Crate>,
    edges: BTreeMap<String, Vec<Dep>>,
}

impl Graph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a crate, or replaces one of the same name.
    pub fn insert(&mut self, krate: Crate) {
        self.crates.insert(krate.name.clone(), krate);
    }

    /// Records that `from` depends on `to`.
    pub fn add_dep(&mut self, from: &str, dep: Dep) {
        let deps = self.edges.entry(from.to_string()).or_default();
        if !deps.contains(&dep) {
            deps.push(dep);
            deps.sort();
        }
    }

    /// Every crate, ordered by name.
    pub fn crates(&self) -> impl Iterator<Item = &Crate> {
        self.crates.values()
    }

    /// The crate named `name`.
    pub fn get(&self, name: &str) -> Option<&Crate> {
        self.crates.get(name)
    }

    /// The workspace-internal dependencies of `name`.
    pub fn deps_of(&self, name: &str) -> &[Dep] {
        self.edges.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Rejects a graph that cannot be published as a whole.
    ///
    /// Both of these fail *late* otherwise — at publish time, with a message about the
    /// dependency rather than about the crate that is wrong — which is exactly the shape
    /// of failure this crate exists to remove.
    ///
    /// With [`ReleaseConfig::explicit_order`] set, the dependencies of the listed crates are
    /// deliberately *not* required to be in the release. That list is a statement that those
    /// crates are somebody else's to publish — a workspace sharing crates with a sibling
    /// release, or a crate that is published separately on its own schedule. Ordering within
    /// the list is still enforced by [`Graph::publish_order`].
    pub fn validate(&self, config: &ReleaseConfig) -> Result<()> {
        if config.explicit_order.is_some() {
            return Ok(());
        }
        for krate in self.crates() {
            if !config.includes(krate) {
                continue;
            }
            for dep in self.deps_of(&krate.name) {
                match self.get(&dep.name) {
                    None => {
                        return Err(Error::Workspace(format!(
                            "{} depends on {}, which is not a crate in this workspace.\n\
                             Either the name is misspelled, or it is an external crate that\n\
                             `ReleaseConfig::crate_prefix` is treating as workspace-internal.",
                            krate.name, dep.name
                        )));
                    }
                    Some(d) if !config.includes(d) => {
                        // A build dependency gets different advice: it cannot be moved to
                        // `[dev-dependencies]`, because the build script genuinely needs it.
                        let fix = match dep.kind {
                            DepKind::Build => format!(
                                "let {dep} be published, or drop the build dependency on it",
                                dep = dep.name
                            ),
                            DepKind::Normal => format!(
                                "let {dep} be published, or move the dependency that needs it\n\
                                 into [dev-dependencies] (a dev-dependency is not part of what a\n\
                                 published crate asks for)",
                                dep = dep.name
                            ),
                        };
                        return Err(Error::Workspace(format!(
                            "{} is published but depends on {} (a {} dependency), which sets\n\
                             `publish = false`. The registry can never satisfy that, so a\n\
                             release of {} cannot succeed. Either {fix}.",
                            krate.name,
                            dep.name,
                            match dep.kind {
                                DepKind::Build => "build",
                                DepKind::Normal => "normal",
                            },
                            krate.name,
                        )));
                    }
                    Some(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Whether `name` is part of this release, per `config`.
    ///
    /// The single place the question is answered, so `validate`, `publish_order` and the
    /// plan's exclusion list can never disagree about what ships. Test-only because nothing
    /// in the library asks the question on its own — `publish_order` and `Plan::excluded`
    /// are the answers — and the tests use it to state the property without re-implementing
    /// the filter.
    #[cfg(test)]
    fn is_published(&self, name: &str, config: &ReleaseConfig) -> bool {
        self.get(name).is_some_and(|c| config.includes(c))
    }

    /// The order to publish in: dependencies first, then dependents.
    ///
    /// A topological sort by Kahn's algorithm, with ties broken by name so the output is
    /// stable across runs.
    ///
    /// When [`ReleaseConfig::explicit_order`] is set, that list is used instead — but only
    /// after being checked against the graph. An explicit order selects *which* crates ship
    /// and states their sequence; it does not get to be wrong about dependencies, so a list
    /// that names a crate before something it depends on is rejected rather than obeyed.
    pub fn publish_order(&self, config: &ReleaseConfig) -> Result<Vec<String>> {
        if let Some(names) = &config.explicit_order {
            return self.explicit_order(names);
        }

        let published: BTreeSet<&str> = self
            .crates
            .values()
            .filter(|c| config.includes(c))
            .map(|c| c.name.as_str())
            .collect();

        // For each candidate, the published dependencies it is still waiting for. Only
        // published crates are in here at all, so an unpublished dependency can never
        // block the queue — it is `validate`'s job to reject that case, not this one's.
        let mut waiting: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for name in &published {
            waiting.insert(
                name,
                self.deps_of(name)
                    .iter()
                    .map(|d| d.name.as_str())
                    .filter(|d| published.contains(d))
                    .collect(),
            );
        }

        let mut order: Vec<String> = Vec::with_capacity(published.len());
        while !waiting.is_empty() {
            // Ready: waiting on nothing. `BTreeMap` iterates in name order, so when
            // several crates become ready together they come out sorted.
            let ready: Vec<&str> = waiting
                .iter()
                .filter(|(_, deps)| deps.is_empty())
                .map(|(name, _)| *name)
                .collect();

            if ready.is_empty() {
                break;
            }

            for name in &ready {
                waiting.remove(name);
                order.push((*name).to_string());
            }

            // Everything emitted is satisfied for whoever was waiting on it.
            for deps in waiting.values_mut() {
                for done in &ready {
                    deps.remove(done);
                }
            }
        }

        if !waiting.is_empty() {
            let mut stuck: Vec<&str> = waiting.keys().copied().collect();
            stuck.sort_unstable();
            return Err(Error::Workspace(format!(
                "the publish order cannot be computed: these crates depend on each other in a \
                 cycle, so no order can satisfy the registry:\n  {}",
                stuck.join(", ")
            )));
        }

        Ok(order)
    }

    /// Uses an explicitly listed order, after checking it against the graph.
    ///
    /// Three things can be wrong with such a list, and each gets its own message because
    /// each has a different fix:
    ///
    /// - it names a crate this workspace does not have (a typo, or a crate that moved);
    /// - it repeats one;
    /// - it puts a crate before something it depends on, which the registry cannot satisfy
    ///   no matter how deliberate the list looks.
    fn explicit_order(&self, names: &[String]) -> Result<Vec<String>> {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for name in names {
            if self.get(name).is_none() {
                return Err(Error::Workspace(format!(
                    "the explicit order names {name:?}, which is not a package in this \
                     workspace.\n\
                     Use the *package* name rather than the binary name: a subcommand \
                     plugin's package is `foo` while its binary is `cargo-foo`.\n\
                     Check the spelling against `cargo metadata --no-deps`."
                )));
            }
            if !seen.insert(name.as_str()) {
                return Err(Error::Workspace(format!(
                    "the explicit order names {name:?} more than once. A crate is published \
                     once per release, so a repeat is a mistake in the list rather than two \
                     publishes."
                )));
            }
        }

        // The graph still has the last word. This is what keeps an explicit order a
        // selection rather than an override: it can say *which* crates, and in what
        // sequence, but not that a dependency may come later than its dependent.
        for (index, name) in names.iter().enumerate() {
            for dep in self.deps_of(name) {
                if !seen.contains(dep.name.as_str()) {
                    // Depends on something outside the list entirely: that crate is not
                    // being published, so `validate` is where it belongs.
                    continue;
                }
                let position = names
                    .iter()
                    .position(|n| n == &dep.name)
                    .expect("a name in `seen` came from `names`");
                if position > index {
                    return Err(Error::Workspace(format!(
                        "the explicit order publishes {name} before {} (position {}), which it \
                         depends on.\nThe registry cannot satisfy that. Swap them.",
                        dep.name,
                        position + 1
                    )));
                }
            }
        }

        Ok(names.to_vec())
    }
}

/// What a release would do, computed without touching the network.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Where `cargo publish` runs, and where the manifests were read from.
    pub root: PathBuf,
    /// The crates to publish, in the order they must go out.
    pub order: Vec<Crate>,
    /// The configuration this plan was computed with.
    ///
    /// Kept because it is what answers [`Plan::excluded`] — which crates were left out is a
    /// question about the config, not a second list to keep in step with `order`.
    config: ReleaseConfig,
    /// Every crate the workspace held, so `excluded` can be derived rather than stored.
    graph: Graph,
}

impl Plan {
    /// Builds a plan from a graph that has already been validated and ordered.
    pub(crate) fn new(
        root: PathBuf,
        graph: Graph,
        order: Vec<String>,
        config: ReleaseConfig,
    ) -> Self {
        let order = order
            .iter()
            .filter_map(|name| graph.get(name).cloned())
            .collect();
        Self {
            root,
            order,
            config,
            graph,
        }
    }

    /// How many crates will be published.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Whether there is nothing to publish.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The crate names in publish order.
    pub fn names(&self) -> Vec<&str> {
        self.order.iter().map(|c| c.name.as_str()).collect()
    }

    /// Crates the workspace holds that this release leaves out.
    ///
    /// Derived from the graph rather than stored, so it cannot fall out of step with
    /// `order`: a crate is excluded exactly when the configuration does not include it.
    /// Reported rather than silently dropped, because "why is my crate not in the release"
    /// is the question this list exists to answer.
    pub fn excluded(&self) -> Vec<&Crate> {
        self.graph
            .crates()
            .filter(|c| !self.config.includes(c))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn krate(name: &str, publish: bool) -> Crate {
        Crate {
            name: name.to_string(),
            version: "0.1.0".to_string(),
            publish,
        }
    }

    fn dep(name: &str) -> Dep {
        Dep {
            name: name.to_string(),
            kind: DepKind::Normal,
        }
    }

    fn build_dep(name: &str) -> Dep {
        Dep {
            name: name.to_string(),
            kind: DepKind::Build,
        }
    }

    /// The default configuration, which honours `publish = false`.
    fn config() -> ReleaseConfig {
        ReleaseConfig::default()
    }

    /// A graph with the given crates and `a -> b` edges.
    fn graph(crates: &[(&str, bool)], edges: &[(&str, &str)]) -> Graph {
        let mut g = Graph::new();
        for (name, publish) in crates {
            g.insert(krate(name, *publish));
        }
        for (from, to) in edges {
            g.add_dep(from, dep(to));
        }
        g
    }

    /// The property the whole module exists for.
    fn assert_dependencies_come_first(g: &Graph, order: &[String], config: &ReleaseConfig) {
        let mut seen = BTreeSet::new();
        for name in order {
            for d in g.deps_of(name) {
                if g.is_published(&d.name, config) {
                    assert!(
                        seen.contains(d.name.as_str()),
                        "{name} is published before its dependency {}",
                        d.name
                    );
                }
            }
            seen.insert(name.as_str());
        }
    }

    #[test]
    fn a_dependency_is_always_published_before_its_dependent() {
        // The shape that was wrong in practice: object depends on map.
        let g = graph(
            &[
                ("acme-core", true),
                ("acme-archive", true),
                ("acme-map", true),
                ("acme-object", true),
            ],
            &[
                ("acme-archive", "acme-core"),
                ("acme-map", "acme-core"),
                ("acme-map", "acme-archive"),
                ("acme-object", "acme-map"),
            ],
        );
        g.validate(&config()).unwrap();
        let order = g.publish_order(&config()).unwrap();
        assert_dependencies_come_first(&g, &order, &config());
    }

    #[test]
    fn object_is_scheduled_after_map() {
        // The regression, stated directly.
        let g = graph(
            &[("acme-map", true), ("acme-object", true)],
            &[("acme-object", "acme-map")],
        );
        let order = g.publish_order(&config()).unwrap();
        let map = order.iter().position(|c| c == "acme-map").unwrap();
        let object = order.iter().position(|c| c == "acme-object").unwrap();
        assert!(map < object);
    }

    #[test]
    fn independent_crates_come_out_in_name_order() {
        let g = graph(&[("acme-b", true), ("acme-a", true), ("acme-c", true)], &[]);
        assert_eq!(
            g.publish_order(&config()).unwrap(),
            vec!["acme-a", "acme-b", "acme-c"]
        );
    }

    #[test]
    fn unpublished_crates_are_never_in_the_order() {
        let g = graph(
            &[("acme-core", true), ("acme-cli", false)],
            &[("acme-cli", "acme-core")],
        );
        assert_eq!(g.publish_order(&config()).unwrap(), vec!["acme-core"]);
    }

    #[test]
    fn a_crate_with_publish_false_is_excluded_unless_the_flag_is_ignored() {
        // This is the difference `respect_publish_flag` makes, and it used to make none at
        // all: the field was declared and never read, so setting it changed nothing.
        let g = graph(&[("acme-a", true), ("acme-b", false)], &[]);

        let honouring = config();
        assert!(honouring.includes(g.get("acme-a").unwrap()));
        assert!(!honouring.includes(g.get("acme-b").unwrap()));
        assert_eq!(
            g.publish_order(&honouring).unwrap(),
            vec!["acme-a"],
            "publish = false keeps a crate out"
        );

        let ignoring = ReleaseConfig {
            respect_publish_flag: false,
            ..config()
        };
        assert!(ignoring.includes(g.get("acme-b").unwrap()));
        assert_eq!(
            g.publish_order(&ignoring).unwrap(),
            vec!["acme-a", "acme-b"],
            "ignoring the flag publishes everything"
        );
    }

    #[test]
    fn a_cycle_is_reported_with_the_crates_in_it() {
        let g = graph(
            &[("acme-a", true), ("acme-b", true)],
            &[("acme-a", "acme-b"), ("acme-b", "acme-a")],
        );
        let err = g.publish_order(&config()).unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");
        assert!(err.contains("acme-a") && err.contains("acme-b"), "{err}");
    }

    #[test]
    fn publishing_a_crate_that_depends_on_an_excluded_one_is_rejected() {
        let g = graph(
            &[("acme-cli", false), ("acme-lib", true)],
            &[("acme-lib", "acme-cli")],
        );
        let err = g.validate(&config()).unwrap_err().to_string();
        assert!(
            err.contains("acme-lib") && err.contains("acme-cli"),
            "{err}"
        );
        assert!(err.contains("publish = false"), "{err}");
        assert!(err.contains("[dev-dependencies]"), "{err}");
    }

    #[test]
    fn a_build_dependency_gets_advice_that_fits_a_build_dependency() {
        // Moving a build dependency to [dev-dependencies] would break the build script, so
        // the message must not suggest it.
        let mut g = graph(&[("acme-cli", false), ("acme-lib", true)], &[]);
        g.add_dep("acme-lib", build_dep("acme-cli"));
        let err = g.validate(&config()).unwrap_err().to_string();
        assert!(err.contains("build dependency"), "{err}");
        assert!(
            !err.contains("[dev-dependencies]"),
            "a build script genuinely needs it: {err}"
        );
    }

    #[test]
    fn a_dependency_outside_the_workspace_is_rejected_with_a_hint() {
        let g = graph(&[("acme-core", true)], &[("acme-core", "acme-thirdparty")]);
        let err = g.validate(&config()).unwrap_err().to_string();
        assert!(err.contains("acme-thirdparty"), "{err}");
        assert!(err.contains("crate_prefix"), "{err}");
    }

    #[test]
    fn a_dev_dependency_is_not_an_edge_at_all() {
        // A dev-dependency is not part of what a published crate asks the registry for, so
        // it is dropped while reading the manifest rather than modelled here. The visible
        // consequence is that a cycle through one is legal, and that a normal edge in the
        // opposite direction still sorts.
        let g = graph(
            &[("acme-a", true), ("acme-b", true)],
            &[("acme-a", "acme-b")],
        );
        assert_eq!(
            g.publish_order(&config()).unwrap(),
            vec!["acme-b", "acme-a"]
        );
    }

    #[test]
    fn an_empty_graph_is_not_a_failure() {
        let g = Graph::new();
        g.validate(&config()).unwrap();
        assert!(g.publish_order(&config()).unwrap().is_empty());
    }

    #[test]
    fn a_dependency_declared_twice_is_recorded_once() {
        let mut g = Graph::new();
        g.insert(krate("acme-a", true));
        g.insert(krate("acme-b", true));
        g.add_dep("acme-a", dep("acme-b"));
        g.add_dep("acme-a", dep("acme-b"));
        assert_eq!(g.deps_of("acme-a").len(), 1);
    }

    #[test]
    fn a_plan_derives_its_exclusions_from_the_configuration() {
        let g = graph(&[("acme-a", true), ("acme-cli", false)], &[]);
        let plan = Plan::new(PathBuf::from("."), g, vec!["acme-a".to_string()], config());
        assert_eq!(plan.names(), vec!["acme-a"]);
        let excluded = plan.excluded();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].name, "acme-cli");
    }

    /// A configuration that names exactly one crate, whatever the manifests say.
    fn only(names: &[&str]) -> ReleaseConfig {
        ReleaseConfig {
            explicit_order: Some(names.iter().map(|n| (*n).to_string()).collect()),
            ..config()
        }
    }

    #[test]
    fn an_explicit_order_publishes_a_crate_that_sets_publish_false() {
        // The case this exists for: a tool that ships as an installed binary declares
        // `publish = false`, correctly, and then has to be able to release itself.
        let g = graph(&[("acme-tool", false)], &[]);
        let config = only(&["acme-tool"]);

        assert!(config.includes(g.get("acme-tool").unwrap()));
        assert_eq!(g.publish_order(&config).unwrap(), vec!["acme-tool"]);
        g.validate(&config).unwrap();
    }

    #[test]
    fn an_explicit_order_selects_and_excludes_everything_else() {
        let g = graph(&[("acme-a", true), ("acme-b", true), ("acme-c", true)], &[]);
        let config = only(&["acme-b"]);
        assert_eq!(g.publish_order(&config).unwrap(), vec!["acme-b"]);
        assert!(!config.includes(g.get("acme-a").unwrap()));
    }

    #[test]
    fn an_explicit_order_still_has_to_respect_dependencies() {
        // The point of checking: an explicit list selects and sequences, but it does not get
        // to be wrong about the graph.
        let g = graph(
            &[("acme-map", true), ("acme-object", true)],
            &[("acme-object", "acme-map")],
        );
        let err = g
            .publish_order(&only(&["acme-object", "acme-map"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("acme-object"), "{err}");
        assert!(err.contains("depends on"), "{err}");
        assert!(err.contains("Swap them"), "{err}");
    }

    #[test]
    fn a_correct_explicit_order_is_accepted() {
        let g = graph(
            &[("acme-map", true), ("acme-object", true)],
            &[("acme-object", "acme-map")],
        );
        let order = g
            .publish_order(&only(&["acme-map", "acme-object"]))
            .unwrap();
        assert_eq!(order, vec!["acme-map", "acme-object"]);
    }

    #[test]
    fn an_explicit_order_naming_a_crate_that_does_not_exist_is_rejected() {
        let g = graph(&[("acme-a", true)], &[]);
        let err = g
            .publish_order(&only(&["acme-typo"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("acme-typo"), "{err}");
        assert!(err.contains("not a package in this workspace"), "{err}");
        assert!(err.contains("binary name"), "{err}");
    }

    #[test]
    fn an_explicit_order_naming_a_crate_twice_is_rejected() {
        let g = graph(&[("acme-a", true)], &[]);
        let err = g
            .publish_order(&only(&["acme-a", "acme-a"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("more than once"), "{err}");
    }

    #[test]
    fn an_explicit_order_may_leave_a_dependency_to_someone_else() {
        // A workspace can share crates with a sibling release. Naming only the dependent is
        // a statement that the dependency is not this release's business -- so `validate`
        // must not complain that it is unpublished, which is what it does when the order is
        // derived.
        let g = graph(
            &[("acme-core", false), ("acme-app", true)],
            &[("acme-app", "acme-core")],
        );
        assert!(
            g.validate(&config()).is_err(),
            "the derived order rejects it"
        );
        assert!(
            g.validate(&only(&["acme-app"])).is_ok(),
            "an explicit order allows it"
        );
        assert_eq!(
            g.publish_order(&only(&["acme-app"])).unwrap(),
            vec!["acme-app"]
        );
    }
}
