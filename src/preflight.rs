//! The checks a release should pass before it touches the registry.
//!
//! These used to be three shell snippets in a workflow file, each with its own `set -euo
//! pipefail` and its own hand-rolled `::error::`. They are here because they are the part
//! of a release that is pure policy: "the tag must match the version", "there must be a
//! token", "do not print the token". Policy is worth a test, and a shell snippet in a
//! `run:` block cannot have one.
//!
//! Nothing here reads the environment on its own. The CLI passes the values in, so a test
//! can exercise every branch without arranging variables — and so a library user is never
//! surprised by a check that consults the ambient environment behind their back.

use std::path::Path;

use crate::error::Error;
use crate::error::Result;
use crate::manifest::manifest_path;
use crate::manifest::Manifest;

/// The name of the variable the token is read from.
pub const TOKEN_VAR: &str = "CARGO_REGISTRY_TOKEN";

/// Checks that `tag` names the version the manifest declares.
///
/// A leading `v` is stripped, because a Git tag conventionally carries one and the manifest
/// cannot. Returns the version on success, so a caller that wants to report it does not
/// read the manifest twice.
///
/// The version is taken from `[workspace.package]` when there is one, and otherwise from
/// the root package itself. Both shapes are ordinary — a workspace root can be a virtual
/// manifest that only declares members, or a real crate that also happens to be the root —
/// and reading only the first is the same class of mistake as the hand-written order this
/// crate exists to replace: correct for the case it was written against, wrong for the
/// next one, and quiet about it either way.
///
/// This is worth failing on rather than warning about: a tag that disagrees with the
/// manifest publishes a version nobody asked for under a name that then lies about what it
/// contains, and crates.io does not allow that to be taken back.
pub fn check_tag(root: &Path, tag: &str) -> Result<String> {
    let path = manifest_path(root);
    let manifest = Manifest::read(&path)?;

    let inherited = manifest
        .workspace
        .as_ref()
        .and_then(|w| w.package.as_ref())
        .and_then(|p| p.version.clone());
    let literal = manifest
        .package
        .as_ref()
        .and_then(|p| p.version.as_ref())
        .and_then(|v| v.value().cloned());

    // A literal on the root package wins over the inherited one. A manifest cannot
    // meaningfully state both, so the only thing that matters is not silently ignoring one.
    let version = literal.or(inherited).ok_or_else(|| {
        Error::manifest(
            &path,
            "declares no version to compare the tag against: expected a `version` in \
             [package], or one in [workspace.package] for members to inherit",
        )
    })?;

    let tag_version = tag.strip_prefix('v').unwrap_or(tag);
    if tag_version != version {
        // The fix is named without assuming where the version lives. A single package keeps
        // it in `[package]`; a workspace keeps it in `[workspace.package]` and inherits it.
        // Telling a single-package crate to edit keys it does not have sends the reader
        // looking for something that is not there.
        return Err(Error::Preflight(format!(
            "tag {tag:?} does not match the manifest version {version:?}.\n\
             The tag names the release and the manifest names what it contains, so they have\n\
             to agree. Bump the version in Cargo.toml — `[workspace.package]` and the\n\
             `[workspace.dependencies]` keys together, if this is a workspace — or move the tag."
        )));
    }

    Ok(version)
}

/// Checks that a token is present, without revealing it.
///
/// The value never reaches a command line and never reaches a log: a token on an argument
/// list is visible to every process on the machine, and one in a log is leaked to everyone
/// who can read the run. Only the length is reported, which is enough to tell "unset" from
/// "set to something wrong".
pub fn check_token(token: Option<&str>) -> Result<()> {
    match token {
        None => Err(Error::Preflight(format!(
            "{TOKEN_VAR} is not set.\n\
             Add it under Settings -> Secrets and variables -> Actions, named exactly\n\
             {TOKEN_VAR}, and pass it to this step through `env:` — never as an argument."
        ))),
        Some(t) if t.trim().is_empty() => Err(Error::Preflight(format!(
            "{TOKEN_VAR} is set but empty.\n\
             An empty secret is the failure that looks like a permissions problem: the\n\
             variable exists, so nothing reports it as missing, and the registry rejects the\n\
             upload instead."
        ))),
        Some(t) => {
            println!(
                "{TOKEN_VAR} is present ({} characters, not printed)",
                t.len()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            format!(
                "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.package]\nversion = \"{version}\"\n"
            ),
        )
        .unwrap();
        dir
    }

    /// A crate that is its own workspace root, with a literal version.
    ///
    /// The other ordinary shape, and the one this crate itself has: no virtual manifest, no
    /// `[workspace.package]`, just a package whose version the tag has to match.
    fn standalone(manifest: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
        dir
    }

    #[test]
    fn a_matching_tag_passes_and_reports_the_version() {
        let dir = workspace("0.0.5");
        assert_eq!(check_tag(dir.path(), "0.0.5").unwrap(), "0.0.5");
    }

    #[test]
    fn a_v_prefixed_tag_matches_the_bare_version() {
        // The convention the workflow uses, and the one the manifest cannot carry.
        let dir = workspace("0.0.5");
        assert_eq!(check_tag(dir.path(), "v0.0.5").unwrap(), "0.0.5");
    }

    #[test]
    fn a_tag_that_disagrees_fails_and_names_both_sides() {
        let dir = workspace("0.0.5");
        let err = check_tag(dir.path(), "v0.0.4").unwrap_err().to_string();
        assert!(err.contains("0.0.4") && err.contains("0.0.5"), "{err}");
    }

    #[test]
    fn only_a_leading_v_is_stripped() {
        // A tag like `version-1.0.0` is not a version, and guessing at it would be worse
        // than reporting the mismatch.
        let dir = workspace("1.0.0");
        assert!(check_tag(dir.path(), "version-1.0.0").is_err());
    }

    #[test]
    fn a_standalone_crate_is_checked_against_its_own_version() {
        // The regression: only `[workspace.package] version` used to be read, so a crate
        // that is its own workspace root could not have its tag checked at all.
        let dir = standalone("[package]\nname = \"solo\"\nversion = \"0.3.1\"\n");
        assert_eq!(check_tag(dir.path(), "v0.3.1").unwrap(), "0.3.1");
        assert!(check_tag(dir.path(), "v0.3.0").is_err());
    }

    #[test]
    fn a_mismatch_on_a_standalone_crate_does_not_point_at_workspace_keys() {
        // A single package has no `[workspace.package]` and no `[workspace.dependencies]`.
        // Naming them sends the reader looking for tables that are not in the file, so the
        // advice has to work for both shapes.
        let dir = standalone("[package]\nname = \"solo\"\nversion = \"0.3.1\"\n");
        let err = check_tag(dir.path(), "v0.3.0").unwrap_err().to_string();
        assert!(err.contains("Cargo.toml"), "{err}");
        assert!(err.contains("if this is a workspace"), "{err}");
        assert!(err.contains("or move the tag"), "{err}");
    }

    #[test]
    fn a_manifest_with_no_version_at_all_is_an_error_rather_than_a_pass() {
        let dir = standalone("[workspace]\nmembers = [\"crates/*\"]\n");
        let err = check_tag(dir.path(), "1.0.0").unwrap_err().to_string();
        assert!(err.contains("[workspace.package]"), "{err}");
        assert!(err.contains("[package]"), "{err}");
    }

    #[test]
    fn a_present_token_passes() {
        assert!(check_token(Some("cargo-secret-value")).is_ok());
    }

    #[test]
    fn an_absent_token_fails_with_the_exact_variable_name() {
        let err = check_token(None).unwrap_err().to_string();
        assert!(err.contains(TOKEN_VAR), "{err}");
        assert!(err.contains("Secrets and variables"), "{err}");
    }

    #[test]
    fn an_empty_token_fails_rather_than_passing_as_present() {
        // The subtle one: the variable exists, so a naive presence check passes and the
        // failure surfaces later as a permission error from the registry.
        let err = check_token(Some("   ")).unwrap_err().to_string();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn the_token_value_is_never_returned_or_printed() {
        // A guard on the shape of the API: `check_token` returns `()`, so there is no way
        // for a caller to accidentally put the secret into a message or a file.
        let secret = "cargo-do-not-print-me";
        let result: Result<()> = check_token(Some(secret));
        assert!(result.is_ok());
    }
}
