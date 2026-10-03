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

/// Checks that `tag` names the version the workspace manifest declares.
///
/// A leading `v` is stripped, because a Git tag conventionally carries one and the manifest
/// cannot. Returns the version on success, so a caller that wants to report it does not
/// read the manifest twice.
///
/// This is worth failing on rather than warning about: a tag that disagrees with the
/// manifest publishes a version nobody asked for under a name that now lies about what
/// it contains, and crates.io does not allow it to be taken back.
pub fn check_tag(root: &Path, tag: &str) -> Result<String> {
    let path = manifest_path(root);
    let manifest = Manifest::read(&path)?;

    let version = manifest
        .workspace
        .as_ref()
        .and_then(|w| w.package.as_ref())
        .and_then(|p| p.version.clone())
        .ok_or_else(|| {
            Error::manifest(
                &path,
                "has no [workspace.package] version to compare the tag against",
            )
        })?;

    let tag_version = tag.strip_prefix('v').unwrap_or(tag);
    if tag_version != version {
        return Err(Error::Preflight(format!(
            "tag {tag:?} does not match workspace version {version:?}.\n\
             The tag names the release and the manifest names what it contains, so they have\n\
             to agree: bump [workspace.package] version and the version keys in\n\
             [workspace.dependencies] together, or move the tag."
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
    fn a_workspace_without_a_version_is_an_error_rather_than_a_pass() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .unwrap();
        let err = check_tag(dir.path(), "1.0.0").unwrap_err().to_string();
        assert!(err.contains("[workspace.package]"), "{err}");
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
