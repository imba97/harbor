//! GitHub Actions' workflow commands, when they are wanted.
//!
//! A release that fails should say *why* in the place a reviewer looks: an annotation on
//! the run, not a line buried in a log. The commands are plain lines on stdout, so they are
//! harmless anywhere else — but they are only emitted when `GITHUB_ACTIONS` is set, so a
//! local run does not fill a terminal with `::error::`.

use std::cell::Cell;
use std::sync::OnceLock;

/// Whether workflow commands should be emitted.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GITHUB_ACTIONS").is_some())
}

thread_local! {
    /// How many groups this thread has open.
    ///
    /// Thread-local rather than global, because the only consumer is a test. A release is
    /// single-threaded, so "groups open on this thread" is the whole truth for it, whereas a
    /// global counter lets one test observe another's open group when they run in parallel —
    /// a false failure that says nothing about the code.
    ///
    /// Maintained whether or not commands are being emitted, which is what makes it usable
    /// as an assertion: the invariant — every [`group`] is matched by an [`end_group`], even
    /// when the work between them fails — has nothing to do with whether a log is written.
    static OPEN_GROUPS: Cell<usize> = const { Cell::new(0) };
}

/// How many groups are open on this thread right now.
///
/// Zero between crates is the invariant; a non-zero value means an unbalanced
/// `::group::`/`::endgroup::` pair, which is how a failed crate used to leave a CI log
/// nested inside a group that never closed.
pub fn open_groups() -> usize {
    OPEN_GROUPS.with(Cell::get)
}

/// Escapes the characters GitHub reads specially in a command's message.
fn escape(message: &str) -> String {
    message
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Opens a collapsible log group. Every call must be paired with [`end_group`].
pub fn group(title: &str) {
    OPEN_GROUPS.with(|open| open.set(open.get().saturating_add(1)));
    if enabled() {
        println!("::group::{}", escape(title));
    }
}

/// Closes the group opened by [`group`].
pub fn end_group() {
    // Saturating rather than wrapping: an unmatched `end_group` is a bug, but it should not
    // turn into a count of nearly `usize::MAX` and look like thousands of open groups.
    OPEN_GROUPS.with(|open| open.set(open.get().saturating_sub(1)));
    if enabled() {
        println!("::endgroup::");
    }
}

/// Reports a failure that should be visible on the run summary.
pub fn error(message: &str) {
    if enabled() {
        println!("::error::{}", escape(message));
    }
}

/// Reports something worth knowing that is not a failure.
pub fn notice(message: &str) {
    if enabled() {
        println!("::notice::{}", escape(message));
    }
}

/// Reports a warning.
pub fn warning(message: &str) {
    if enabled() {
        println!("::warning::{}", escape(message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_and_percent_are_escaped_so_a_command_stays_one_line() {
        assert_eq!(escape("a\nb"), "a%0Ab");
        assert_eq!(escape("100%"), "100%25");
        assert_eq!(escape("a\r\nb"), "a%0D%0Ab");
    }

    #[test]
    fn escaping_does_not_double_escape_the_replacement() {
        // `%` is escaped first, so the `%0A` produced for a newline is not itself escaped.
        assert_eq!(escape("\n"), "%0A");
    }

    #[test]
    fn a_group_opens_and_closes_without_drifting() {
        // The counter itself, tested where it lives: whatever else is wrong, this pins that
        // the two functions are each other's inverse.
        let before = open_groups();
        group("a");
        assert_eq!(open_groups(), before + 1);
        end_group();
        assert_eq!(open_groups(), before);

        // Nested groups too, since a release can nest one inside another.
        group("outer");
        group("inner");
        assert_eq!(open_groups(), before + 2);
        end_group();
        end_group();
        assert_eq!(open_groups(), before);
    }

    #[test]
    fn an_unmatched_end_group_does_not_underflow() {
        let before = open_groups();
        assert_eq!(
            before, 0,
            "this test relies on starting from a closed state"
        );
        end_group();
        assert_eq!(open_groups(), 0);
    }
}
