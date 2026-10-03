//! Turning a directory into a plan: which crates exist, and in what order they ship.
//!
//! Discovery follows the workspace's own `members` list. Globbing `crates/*` instead would
//! be the same thing right up until a crate lives somewhere else — which is exactly what
//! this crate's tests do — and then it would silently miss it.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use crate::config::ReleaseConfig;
use crate::error::Error;
use crate::error::Result;
use crate::graph::Graph;
use crate::graph::Plan;
use crate::manifest::manifest_path;
use crate::manifest::Manifest;

/// Reads `root` and computes what a release would publish, in order.
///
/// Nothing here touches the network: this is the answer to "what would happen", and every
/// guard against an unpublishable workspace is applied before a plan is returned.
pub fn plan(root: &Path, config: &ReleaseConfig) -> Result<Plan> {
    let graph = load(root, config)?;
    graph.validate(config)?;

    let order = graph.publish_order(config)?;

    // The graph itself goes into the plan, which derives the exclusion list from it on
    // demand rather than storing a second copy of every crate it leaves out.
    Ok(Plan::new(root.to_path_buf(), graph, order, config.clone()))
}

/// Reads every member of the release into a graph.
///
/// "Every member" rather than "every workspace member", because a crate that is its own root
/// and declares no `[workspace]` table is a perfectly ordinary release — the commonest kind,
/// even. Such a manifest names exactly one package: itself.
pub fn load(root: &Path, config: &ReleaseConfig) -> Result<Graph> {
    let root_manifest_path = manifest_path(root);
    let root_manifest = Manifest::read(&root_manifest_path)?;

    // A virtual manifest names its members in `[workspace] members`. A real crate that is its
    // own root names none, and its single member is its own directory.
    //
    // Treating the missing table as an error is what made `harbor plan` refuse a
    // single-package crate outright, which is the shape most crates have: only a workspace
    // that was deliberately split into several packages has the table at all.
    let workspace = root_manifest.workspace.as_ref();
    if workspace.is_none() && root_manifest.package.is_none() {
        return Err(Error::manifest(
            &root_manifest_path,
            "is neither a workspace (no [workspace] members) nor a package (no [package] \
             name), so there is nothing to release",
        ));
    }

    if workspace.is_some_and(|w| w.members.is_empty()) {
        return Err(Error::manifest(
            &root_manifest_path,
            "has an empty [workspace] members list",
        ));
    }

    let workspace_version = workspace
        .and_then(|w| w.package.as_ref())
        .and_then(|p| p.version.as_deref());
    let workspace_publish = workspace
        .and_then(|w| w.package.as_ref())
        .and_then(|p| p.publish.as_ref())
        .map(crate::manifest::Publish::allows);

    // Read every member first, then resolve dependencies against that set.
    //
    // The two passes matter because "is this dependency a crate in this release" cannot be
    // answered by a name prefix. With an empty prefix — the default, and the right default —
    // *every* name matches, so `clap` looks like a sibling crate and a release of a crate
    // that has any dependency at all fails to even compute a plan. The set of members is the
    // only honest answer, and it is not known until every manifest has been read.
    let mut graph = Graph::new();
    let mut packages: Vec<(String, Vec<crate::graph::Dep>)> = Vec::new();
    for dir in member_dirs(root, workspace, &root_manifest_path)? {
        let path = manifest_path(&dir);
        let manifest = Manifest::read(&path)?;

        // A virtual manifest has no `[package]`, and contributes only its members. Those
        // are already covered: `members` is read from the root, not from here.
        if manifest.package.is_none() {
            continue;
        }

        let krate = manifest.to_crate(&path, workspace_version, workspace_publish)?;
        if !krate.name.starts_with(&config.crate_prefix) {
            // Not part of the family this release is about. Skipped before it can become a
            // dependency edge that `validate` would then complain about.
            continue;
        }
        let name = krate.name.clone();
        let deps = manifest.internal_deps(&config.crate_prefix);
        graph.insert(krate);
        packages.push((name, deps));
    }

    // Owned names, because the set is consulted while the graph is being mutated: a
    // `&str` borrowed from `graph` cannot outlive an `add_dep` that needs it mutably.
    let members: BTreeSet<String> = graph.crates().map(|c| c.name.clone()).collect();
    for (name, deps) in packages {
        for dep in deps {
            if !members.contains(&dep.name) {
                // External. `internal_deps` matched it on the prefix, which is a filter and
                // not a membership test; a workspace with no shared prefix necessarily
                // brings its whole dependency tree through here, and none of it belongs on
                // a dependency edge.
                continue;
            }
            graph.add_dep(&name, dep);
        }
    }

    // `crates()` is not `ExactSizeIterator`, so emptiness is asked of the iterator itself
    // rather than through a `Graph` accessor that existed only for this one call.
    if graph.crates().next().is_none() {
        let what = if config.crate_prefix.is_empty() {
            "there is no crate that can be published".to_string()
        } else {
            format!(
                "no crates match the prefix {:?}.\n\
                 Set `ReleaseConfig::crate_prefix` if this release's crates are not named \
                 that way, or leave it empty to consider every crate.",
                config.crate_prefix
            )
        };
        return Err(Error::Workspace(format!(
            "{what} (under {}).",
            root.display()
        )));
    }

    Ok(graph)
}

/// The directories that hold the manifests taking part in this release.
///
/// A root manifest names its members one of two ways, and both are ordinary: a virtual
/// manifest lists them in `[workspace] members`, while a single-package crate has exactly
/// one — its own directory, which it never has to say out loud.
fn member_dirs(
    root: &Path,
    workspace: Option<&crate::manifest::Workspace>,
    root_manifest_path: &Path,
) -> Result<Vec<PathBuf>> {
    let Some(workspace) = workspace else {
        return Ok(vec![root.to_path_buf()]);
    };
    let patterns = workspace.members.clone();
    let excludes = workspace.exclude.clone();

    // Compiled once. These used to be rebuilt inside `is_excluded` for every candidate
    // against every pattern, which is the same patterns compiled over and over for an
    // answer that cannot change.
    let excludes: Vec<glob::Pattern> = excludes
        .iter()
        .map(|pattern| {
            glob::Pattern::new(&join_for_glob(root, pattern).map_err(|what| {
                Error::manifest(root_manifest_path, format!("exclude pattern {what}"))
            })?)
            .map_err(|e| {
                Error::manifest(
                    root_manifest_path,
                    format!("exclude pattern {pattern:?}: {e}"),
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut dirs: Vec<PathBuf> = Vec::new();

    for pattern in &patterns {
        let joined = join_for_glob(root, pattern).map_err(|what| {
            Error::manifest(root_manifest_path, format!("members pattern {what}"))
        })?;
        let matches = glob::glob(&joined).map_err(|e| {
            Error::manifest(
                root_manifest_path,
                format!("members pattern {pattern:?}: {e}"),
            )
        })?;

        let mut found = 0usize;
        for entry in matches {
            let path = entry.map_err(|e| {
                Error::manifest(
                    root_manifest_path,
                    format!("members pattern {pattern:?}: {e}"),
                )
            })?;
            if !path.is_dir() || !manifest_path(&path).is_file() {
                continue;
            }
            if is_excluded(&path, &excludes) {
                continue;
            }
            found += 1;
            dirs.push(path);
        }

        if found == 0 {
            // A pattern matching nothing is a typo, and treating it as "no crates" is how a
            // crate silently drops out of a release.
            return Err(Error::manifest(
                root_manifest_path,
                format!(
                    "members pattern {pattern:?} matched no crate containing a Cargo.toml.\n\
                     A pattern that matches nothing is almost always a move or a rename that\n\
                     was not finished."
                ),
            ));
        }
    }

    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

/// Joins a workspace-relative pattern onto `root` and renders it for `glob`.
///
/// The `glob` crate matches against a `&str`, so the path has to become one. That is a real
/// conversion rather than a formality: a path that is not valid UTF-8 cannot be expressed as
/// a pattern at all, and `to_string_lossy` would quietly substitute U+FFFD and produce a
/// pattern that matches nothing — which then reads as "your members list is wrong". An
/// error naming the offending path is worth far more than that.
///
/// Backslashes are normalised because the pattern itself comes from `Cargo.toml`, where a
/// separator is always `/`, while `Path::join` on Windows produces `\`.
fn join_for_glob(root: &Path, pattern: &str) -> std::result::Result<String, String> {
    let joined = root.join(pattern);
    let text = joined
        .to_str()
        .ok_or_else(|| format!("{pattern:?} resolves to a path that is not valid UTF-8"))?;
    Ok(text.replace('\\', "/"))
}

/// Whether `dir` is inside one of the workspace's already-compiled `exclude` patterns.
fn is_excluded(dir: &Path, excludes: &[glob::Pattern]) -> bool {
    if excludes.is_empty() {
        return false;
    }
    let Some(text) = dir.to_str() else {
        return false;
    };
    let text = text.replace('\\', "/");
    excludes.iter().any(|pattern| pattern.matches(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a workspace on disk and returns its root.
    ///
    /// A real directory rather than a mock: reading manifests is what this module does, and
    /// the interesting failures (a pattern that matches nothing, a version that inherits
    /// nothing) only exist on a filesystem.
    fn workspace(root_manifest: &str, members: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), root_manifest).unwrap();
        for (rel, manifest) in members {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("Cargo.toml"), manifest).unwrap();
        }
        dir
    }

    fn config() -> ReleaseConfig {
        ReleaseConfig {
            crate_prefix: "acme-".to_string(),
            ..ReleaseConfig::default()
        }
    }

    const ROOT: &str = r#"
[workspace]
members = ["crates/*"]

[workspace.package]
version = "0.0.5"
"#;

    fn member(name: &str, deps: &str) -> String {
        format!(
            "[package]\nname = \"{name}\"\nversion.workspace = true\n\n[dependencies]\n{deps}\n"
        )
    }

    #[test]
    fn plans_a_small_workspace_in_dependency_order() {
        let dir = workspace(
            ROOT,
            &[
                ("crates/acme-core", &member("acme-core", "")),
                (
                    "crates/acme-map",
                    &member("acme-map", "acme-core.workspace = true"),
                ),
                (
                    "crates/acme-object",
                    &member("acme-object", "acme-map.workspace = true"),
                ),
            ],
        );
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.names(), vec!["acme-core", "acme-map", "acme-object"]);
        assert!(plan.excluded().is_empty());
    }

    #[test]
    fn version_comes_from_the_workspace_when_inherited() {
        let dir = workspace(ROOT, &[("crates/acme-core", &member("acme-core", ""))]);
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.order[0].version, "0.0.5");
    }

    #[test]
    fn a_crate_with_publish_false_is_excluded_and_reported() {
        let dir = workspace(
            ROOT,
            &[
                ("crates/acme-core", &member("acme-core", "")),
                (
                    "crates/acme-cli",
                    "[package]\nname = \"acme-cli\"\nversion.workspace = true\npublish = false\n\n[dependencies]\nacme-core.workspace = true\n",
                ),
            ],
        );
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.names(), vec!["acme-core"]);
        assert_eq!(plan.excluded().len(), 1);
        assert_eq!(plan.excluded()[0].name, "acme-cli");
    }

    #[test]
    fn a_pattern_that_matches_nothing_is_an_error() {
        let dir = workspace("[workspace]\nmembers = [\"crates/*\"]\n", &[]);
        let err = plan(dir.path(), &config()).unwrap_err().to_string();
        assert!(err.contains("matched no crate"), "{err}");
    }

    #[test]
    fn a_member_outside_crates_is_found() {
        // The reason members are globbed rather than `crates/*` hardcoded.
        let dir = workspace(
            "[workspace]\nmembers = [\"crates/*\", \"scripts/release\"]\n\n[workspace.package]\nversion = \"1.0.0\"\n",
            &[
                ("crates/acme-core", &member("acme-core", "")),
                (
                    "scripts/release",
                    "[package]\nname = \"acme-release\"\nversion.workspace = true\npublish = false\n",
                ),
            ],
        );
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.names(), vec!["acme-core"]);
        assert_eq!(plan.excluded()[0].name, "acme-release");
    }

    #[test]
    fn excluded_members_are_not_read() {
        let dir = workspace(
            "[workspace]\nmembers = [\"crates/*\"]\nexclude = [\"crates/acme-skip\"]\n\n[workspace.package]\nversion = \"1.0.0\"\n",
            &[
                ("crates/acme-core", &member("acme-core", "")),
                ("crates/acme-skip", &member("acme-skip", "")),
            ],
        );
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.names(), vec!["acme-core"]);
        assert!(plan.excluded().iter().all(|c| c.name != "acme-skip"));
    }

    #[test]
    fn a_prefix_that_matches_nothing_says_so() {
        let dir = workspace(ROOT, &[("crates/other", &member("other", ""))]);
        let err = plan(dir.path(), &config()).unwrap_err().to_string();
        assert!(err.contains("crate_prefix"), "{err}");
    }

    #[test]
    fn a_package_that_is_its_own_root_is_a_release_of_one() {
        // The regression: a manifest with `[package]` and no `[workspace]` table was refused
        // outright, which is the shape most crates have — only a deliberately split workspace
        // carries the table at all. It is a release whose single member is itself.
        let dir = workspace(
            "[package]\nname = \"acme-solo\"\nversion = \"1.2.3\"\n",
            &[],
        );
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.names(), vec!["acme-solo"]);
        assert_eq!(plan.order[0].version, "1.2.3");
    }

    #[test]
    fn a_package_with_a_lib_and_two_bins_is_still_one_member() {
        // Target count is irrelevant to how many packages there are: `cargo-bumpp` ships a
        // library and two binaries from a single package.
        let dir = workspace(
            "[package]\nname = \"acme-tool\"\nversion = \"0.3.1\"\n\n[lib]\nname = \"acme_tool\"\n\n[[bin]]\nname = \"acme-tool\"\n\n[[bin]]\nname = \"acme-toolx\"\n",
            &[],
        );
        let plan = plan(dir.path(), &config()).unwrap();
        assert_eq!(plan.names(), vec!["acme-tool"]);
    }

    #[test]
    fn a_manifest_that_is_neither_a_workspace_nor_a_package_is_an_error() {
        // Empty, or a manifest with only unrelated tables: there is genuinely nothing to
        // release, and saying so is better than reporting an empty plan.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[profile.release]\nlto = true\n",
        )
        .unwrap();
        let err = plan(dir.path(), &config()).unwrap_err().to_string();
        assert!(err.contains("neither a workspace"), "{err}");
    }

    #[test]
    fn an_empty_members_list_is_still_an_error() {
        // The table is there and says "no members", which is a mistake rather than a shape.
        let dir = workspace("[workspace]\nmembers = []\n", &[]);
        let err = plan(dir.path(), &config()).unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn a_missing_root_manifest_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = plan(dir.path(), &config()).unwrap_err().to_string();
        assert!(err.contains("cannot read"), "{err}");
    }

    #[test]
    fn a_crate_depending_on_an_unpublished_one_is_rejected_before_anything_runs() {
        let dir = workspace(
            ROOT,
            &[
                (
                    "crates/acme-cli",
                    "[package]\nname = \"acme-cli\"\nversion.workspace = true\npublish = false\n",
                ),
                (
                    "crates/acme-lib",
                    &member("acme-lib", "acme-cli.workspace = true"),
                ),
            ],
        );
        let err = plan(dir.path(), &config()).unwrap_err().to_string();
        assert!(err.contains("publish = false"), "{err}");
    }
}
