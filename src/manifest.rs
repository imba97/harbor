//! Reading a `Cargo.toml` into the few facts a release needs.
//!
//! `serde` + `toml` rather than a hand-written reader. A real parser is both shorter and
//! correct about the things a line scanner gets wrong — multi-line values, a `#` inside a
//! string, dotted keys such as `version.workspace = true` — and reading manifests
//! correctly is most of what this crate does.
//!
//! Unknown keys are ignored rather than rejected. A manifest carries a great deal this
//! crate has no opinion about — `keywords`, `categories`, `[profile.*]` — and refusing to
//! read a file because it mentions something unrecognised would make this crate the thing
//! that breaks a release.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Error;
use crate::error::Result;
use crate::graph::Crate as GraphCrate;
use crate::graph::Dep;
use crate::graph::DepKind;

/// A `Cargo.toml`, as far as a release is concerned.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Manifest {
    /// The `[package]` table, absent in a virtual workspace root.
    pub package: Option<Package>,
    /// The `[workspace]` table, absent in an ordinary member crate.
    pub workspace: Option<Workspace>,
    /// `[dependencies]`.
    pub dependencies: BTreeMap<String, Dependency>,
    /// `[build-dependencies]`.
    #[serde(rename = "build-dependencies")]
    pub build_dependencies: BTreeMap<String, Dependency>,
    /// `[dev-dependencies]`, read only so it can be deliberately ignored by name.
    #[serde(rename = "dev-dependencies")]
    pub dev_dependencies: BTreeMap<String, Dependency>,
}

/// `[package]`.
///
/// `version.workspace = true` is a *nested table* (`version = { workspace = true }`), not a
/// string, so it cannot be read into the same field. It is accepted as a variant of
/// [`Inheritable`] and then refused where it would be resolved, naming the workspace value
/// that is missing, rather than being read as "this crate has no version".
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Package {
    /// The crate's name.
    pub name: Option<String>,
    /// The version, or `version.workspace = true`.
    pub version: Option<Inheritable<String>>,
    /// `publish`, or `publish.workspace = true`, or a list of registries.
    pub publish: Option<Inheritable<Publish>>,
}

/// A field that a manifest may either state or inherit from the workspace.
///
/// The inherited form has to be modelled even when it cannot be resolved, because the
/// alternative is a deserialisation failure that reports a type error about a table where a
/// string was expected — a message that says nothing about the workspace value that is
/// actually missing.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Inheritable<T> {
    /// A literal value.
    Value(T),
    /// `{ workspace = true }`.
    Workspace {
        /// Read so the shape is checked: `{ workspace = false }` is still the inherited
        /// form, and `true` is the only value Cargo accepts.
        workspace: bool,
    },
}

impl<T> Inheritable<T> {
    /// The literal value, if this is not the inherited form.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(v) => Some(v),
            Self::Workspace { .. } => None,
        }
    }

    /// Whether this is the inherited form.
    pub fn is_inherited(&self) -> bool {
        matches!(self, Self::Workspace { .. })
    }
}

/// The `publish` key: a boolean, or a list of registry names to publish to.
///
/// Only `false` matters to a release: `true` and an explicit registry list both mean "this
/// crate ships", and a list says nothing this crate can act on beyond that.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Publish {
    /// `publish = false` / `publish = true`.
    Bool(bool),
    /// `publish = ["my-registry"]`.
    Registries(Vec<String>),
}

impl Publish {
    /// Whether this value allows the crate to be published.
    pub fn allows(&self) -> bool {
        match self {
            Self::Bool(allowed) => *allowed,
            // A crate restricted to named registries still ships; it just does not ship to
            // crates.io by default. Reading it as publishable is the conservative choice,
            // because the alternative silently drops a crate from the release.
            Self::Registries(_) => true,
        }
    }
}

/// `[workspace]`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Workspace {
    /// The member patterns, which may be globs.
    pub members: Vec<String>,
    /// Patterns excluded from `members`.
    pub exclude: Vec<String>,
    /// `[workspace.package]`: values members can inherit.
    pub package: Option<WorkspacePackage>,
}

/// `[workspace.package]`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct WorkspacePackage {
    /// The version members inherit with `version.workspace = true`.
    pub version: Option<String>,
    /// The `publish` members inherit with `publish.workspace = true`. Always written as a
    /// literal: a workspace cannot inherit from itself.
    pub publish: Option<Publish>,
}

/// One `[dependencies]` entry, in any of its spellings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Dependency {
    /// `foo = "1"`.
    Version(String),
    /// `foo = { path = "...", version = "..." }`.
    Detailed(Box<DetailedDependency>),
}

/// The table form of a dependency.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DetailedDependency {
    /// The real package name, when the dependency was renamed.
    pub package: Option<String>,
    /// Whether this is an optional dependency.
    pub optional: bool,
}

impl Dependency {
    /// The package this dependency actually refers to.
    ///
    /// `package` wins when present, because `foo = { package = "bar" }` means the crate is
    /// `bar`: taking the table key would look for a crate that does not exist.
    pub fn package_name<'a>(&'a self, key: &'a str) -> &'a str {
        match self {
            Self::Version(_) => key,
            Self::Detailed(d) => d.package.as_deref().unwrap_or(key),
        }
    }
}

impl Manifest {
    /// Reads and parses `path`.
    pub fn read(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::manifest(path, format!("cannot read: {e}")))?;
        Self::parse(&text, path)
    }

    /// Parses `text`, naming `path` in any error.
    pub fn parse(text: &str, path: &Path) -> Result<Self> {
        toml::from_str(text).map_err(|e| Error::manifest(path, e.to_string()))
    }

    /// Resolves this manifest into a crate, given the workspace's inherited values.
    ///
    /// `workspace_version` and `workspace_publish` come from the root's
    /// `[workspace.package]`; they are what `version.workspace = true` and
    /// `publish.workspace = true` refer to.
    pub fn to_crate(
        &self,
        manifest_path: &Path,
        workspace_version: Option<&str>,
        workspace_publish: Option<bool>,
    ) -> Result<GraphCrate> {
        let package = self
            .package
            .as_ref()
            .ok_or_else(|| Error::manifest(manifest_path, "has no [package] table"))?;
        let name = package.name.clone().ok_or_else(|| {
            Error::manifest(
                manifest_path,
                "has no package.name, so it cannot be published",
            )
        })?;

        // A literal wins over the inherited form. Cargo rejects the combination outright,
        // so the only thing that matters here is not silently ignoring one of them.
        let version = match package.version.as_ref().and_then(Inheritable::value) {
            Some(v) => Some(v.clone()),
            None if package
                .version
                .as_ref()
                .is_some_and(Inheritable::is_inherited) =>
            {
                workspace_version.map(str::to_string)
            }
            None => None,
        }
        .ok_or_else(|| {
            Error::manifest(
                manifest_path,
                format!(
                    "{name} has no version, and does not inherit one from the workspace.\n\
                     Either give it a literal `version`, or add `version` to the root's\n\
                     [workspace.package] table for `version.workspace = true` to resolve against."
                ),
            )
        })?;

        // `publish` is only meaningful as `false`; everything else (absent, `true`, a list
        // of registries) means the crate may be published. An inherited value is resolved
        // the same way, so a workspace can turn publishing off for all of its members.
        let publish = match package.publish.as_ref() {
            Some(p) => match p.value() {
                Some(literal) => literal.allows(),
                None => workspace_publish.unwrap_or(true),
            },
            None => true,
        };

        Ok(GraphCrate {
            name,
            version,
            publish,
        })
    }

    /// The workspace-internal dependencies of this manifest.
    ///
    /// `[dev-dependencies]` is absent on purpose, and so is an `optional` dependency: a
    /// dev-dependency is not part of what a published crate asks the registry for, and
    /// neither is an optional one that is off by default. Neither constrains the order.
    pub fn internal_deps(&self, prefix: &str) -> Vec<Dep> {
        let mut deps: Vec<Dep> = Vec::new();
        for (kind, table) in [
            (DepKind::Normal, &self.dependencies),
            (DepKind::Build, &self.build_dependencies),
        ] {
            for (key, dependency) in table {
                let name = dependency.package_name(key);
                if !name.starts_with(prefix) {
                    continue;
                }
                if let Dependency::Detailed(d) = dependency {
                    if d.optional {
                        continue;
                    }
                }
                deps.push(Dep {
                    name: name.to_string(),
                    kind,
                });
            }
        }
        deps.sort();
        deps.dedup();
        deps
    }
}

/// The absolute path of a manifest, for error messages that name a real file.
pub(crate) fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("Cargo.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(text: &str) -> Manifest {
        Manifest::parse(text, &PathBuf::from("test/Cargo.toml")).unwrap()
    }

    #[test]
    fn reads_a_plain_crate() {
        let m = parse(
            r#"
[package]
name = "acme-core"
version = "0.0.5"
edition = "2021"
"#,
        );
        let c = m.to_crate(&PathBuf::from("t"), None, None).unwrap();
        assert_eq!(c.name, "acme-core");
        assert_eq!(c.version, "0.0.5");
        assert!(
            c.publish,
            "publish is absent, so the crate may be published"
        );
    }

    #[test]
    fn resolves_an_inherited_version_from_the_workspace() {
        let m = parse(
            r#"
[package]
name = "acme-map"
version.workspace = true
"#,
        );
        let c = m
            .to_crate(&PathBuf::from("t"), Some("0.0.5"), None)
            .unwrap();
        assert_eq!(c.version, "0.0.5");
    }

    #[test]
    fn an_inherited_version_with_nothing_to_inherit_is_an_error() {
        let m = parse(
            r#"
[package]
name = "acme-map"
version.workspace = true
"#,
        );
        let err = m.to_crate(&PathBuf::from("t"), None, None).unwrap_err();
        assert!(err.to_string().contains("does not inherit"), "{err}");
    }

    #[test]
    fn a_missing_version_is_an_error_that_names_the_crate() {
        let m = parse("[package]\nname = \"acme-x\"\n");
        let err = m.to_crate(&PathBuf::from("t"), None, None).unwrap_err();
        assert!(err.to_string().contains("acme-x"), "{err}");
    }

    #[test]
    fn publish_false_is_recognised() {
        let m = parse("[package]\nname = \"acme-cli\"\nversion = \"0.0.5\"\npublish = false\n");
        let c = m.to_crate(&PathBuf::from("t"), None, None).unwrap();
        assert!(!c.publish);
    }

    #[test]
    fn publish_can_be_inherited_from_the_workspace() {
        let m = parse("[package]\nname = \"acme-x\"\nversion = \"1\"\npublish.workspace = true\n");
        let c = m.to_crate(&PathBuf::from("t"), None, Some(false)).unwrap();
        assert!(!c.publish);
    }

    #[test]
    fn a_missing_package_table_is_an_error() {
        // A virtual manifest, which cannot itself be published.
        let m = parse("[workspace]\nmembers = [\"crates/*\"]\n");
        let err = m.to_crate(&PathBuf::from("t"), None, None).unwrap_err();
        assert!(err.to_string().contains("[package]"), "{err}");
    }

    #[test]
    fn reads_workspace_members_including_an_exact_directory() {
        let m = parse(
            r#"
[workspace]
members = ["crates/*", "scripts/release"]

[workspace.package]
version = "0.0.5"
"#,
        );
        let ws = m.workspace.unwrap();
        assert_eq!(ws.members, vec!["crates/*", "scripts/release"]);
        assert_eq!(ws.package.unwrap().version.as_deref(), Some("0.0.5"));
    }

    #[test]
    fn internal_deps_reads_both_spellings() {
        let m = parse(
            r#"
[package]
name = "x"

[dependencies]
acme-core = { path = "../acme-core", version = "0.0.5" }
acme-map.workspace = true
"#,
        );
        let names: Vec<String> = m
            .internal_deps("acme-")
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["acme-core", "acme-map"]);
    }

    #[test]
    fn internal_deps_ignores_dev_optional_and_third_party() {
        let m = parse(
            r#"
[package]
name = "x"

[dependencies]
acme-optional = { path = "../o", optional = true }
serde = "1"

[build-dependencies]
acme-build = { path = "../b" }

[dev-dependencies]
acme-dev = { path = "../d" }
"#,
        );
        let deps = m.internal_deps("acme-");
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["acme-build"],
            "only the build dependency counts"
        );
        assert_eq!(deps[0].kind, DepKind::Build);
    }

    #[test]
    fn a_renamed_dependency_resolves_to_the_real_package() {
        let m = parse(
            r#"
[package]
name = "x"

[dependencies]
core = { package = "acme-core", path = "../acme-core" }
"#,
        );
        let names: Vec<String> = m
            .internal_deps("acme-")
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["acme-core"]);
    }

    #[test]
    fn a_prefix_that_does_not_match_leaves_no_dependencies() {
        let m = parse(
            r#"
[package]
name = "x"

[dependencies]
acme-core.workspace = true
"#,
        );
        assert!(m.internal_deps("harbor-").is_empty());
    }

    #[test]
    fn unreadable_toml_names_the_file() {
        let err = Manifest::parse("this is not toml", &PathBuf::from("a/Cargo.toml")).unwrap_err();
        assert!(err.to_string().contains("a/Cargo.toml"), "{err}");
    }
}
