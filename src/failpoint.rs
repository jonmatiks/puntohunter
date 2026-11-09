//! Failpoints for the crash-recovery tests: named points where the process stops,
//! or a commit fails, when the PUNTOHUNTER_FAILPOINT environment variable selects them. Without it every point is
//! inert. They are compiled into the release binary, so the e2e suite tests the artifact it ships.
//!
//! The variable holds a name, which fires at the point's first pass, or `name:n`, which fires at its n-th.

use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

/// The environment variable that selects a failpoint.
pub const VARIABLE: &str = "PUNTOHUNTER_FAILPOINT";

/// The process exits right after a journal commit is written, before any session hears of it.
pub const AFTER_JOURNAL_COMMIT: &str = "after-journal-commit";
/// The process exits after a share's journal commit returned, before its acknowledgement is sent.
pub const BEFORE_ACKNOWLEDGEMENT: &str = "before-acknowledgement";
/// A journal commit fails.
pub const JOURNAL_COMMIT_FAILS: &str = "journal-commit-fails";
/// The process exits once a payout batch's balances and inputs are reserved, before its inputs are locked
/// in the wallet and it is signed.
pub const AFTER_RESERVATION: &str = "after-reservation";
/// The process exits once a payout batch's signed transaction is persisted, before it is broadcast.
pub const AFTER_SIGNING: &str = "after-signing";
/// The process exits once the node took a payout batch's transaction, before the broadcast is recorded.
pub const AFTER_BROADCAST: &str = "after-broadcast";

/// The selected failpoint, its pass to fire at, and the passes so far.
struct Selected {
    name: String,
    at: u64,
    passes: AtomicU64,
}

fn selected() -> Option<&'static Selected> {
    static SELECTED: OnceLock<Option<Selected>> = OnceLock::new();
    SELECTED
        .get_or_init(|| parse(&std::env::var(VARIABLE).ok()?))
        .as_ref()
}

fn parse(value: &str) -> Option<Selected> {
    let (name, at) = match value.split_once(':') {
        Some((name, at)) => (name, at.parse().ok().filter(|&at| at > 0)?),
        None => (value, 1),
    };
    (!name.is_empty()).then(|| Selected {
        name: name.to_owned(),
        at,
        passes: AtomicU64::new(0),
    })
}

/// Whether `name` fires at this pass.
pub fn fires(name: &str) -> bool {
    selected().is_some_and(|selected| fires_at(selected, name))
}

fn fires_at(selected: &Selected, name: &str) -> bool {
    selected.name == name && selected.passes.fetch_add(1, Ordering::Relaxed) + 1 == selected.at
}

/// Exits the process at once if `name` fires: no destructor runs and nothing more is written, as in a crash.
pub fn exit_if(name: &str) {
    if fires(name) {
        tracing::error!(failpoint = name, "failpoint: exiting");
        std::process::exit(101);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_at_the_selected_pass_of_the_selected_point_only() {
        let first = parse("journal-commit-fails").unwrap();
        assert!(!fires_at(&first, "after-journal-commit"));
        assert!(fires_at(&first, "journal-commit-fails"));
        assert!(!fires_at(&first, "journal-commit-fails"), "once");

        let third = parse("after-journal-commit:3").unwrap();
        let passes: Vec<bool> = (0..4)
            .map(|_| fires_at(&third, "after-journal-commit"))
            .collect();
        assert_eq!(passes, [false, false, true, false]);

        for bad in ["", "name:0", "name:x", ":2"] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }
}
