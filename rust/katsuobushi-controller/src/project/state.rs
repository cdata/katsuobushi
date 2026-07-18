//! The lifecycle state machine — pure over [`Status`].
//!
//! `set-status` enforces [`transition_allowed`]; `--force` bypasses it. The
//! graph (design/project.md §4):
//!
//! ```text
//! todo         -> in-progress | cancelled
//! in-progress  -> todo | needs-review | cancelled
//! needs-review -> in-progress | todo | ready | cancelled
//! ready        -> accepted | todo | cancelled
//! accepted     -> (terminal)
//! cancelled    -> (terminal)
//! ```
//!
//! The asymmetry is deliberate: a reviewer bounce from `needs-review` is a
//! light drop to `in-progress`; an owner return from `ready` is a heavier reset
//! to `todo`. `ready -> accepted` is legal *here* but the human-only rule that
//! guards it lives in the skill, not the tool.

use super::model::Status;

/// Whether `from -> to` is a legal lifecycle transition. A no-op (`from == to`)
/// is allowed as an idempotent move. Terminal states have no outgoing edges.
pub fn transition_allowed(from: Status, to: Status) -> bool {
    use Status::*;
    if from == to {
        return true;
    }
    match (from, to) {
        (Todo, InProgress) => true,
        (InProgress, Todo | NeedsReview) => true,
        (NeedsReview, InProgress | Todo | Ready) => true,
        (Ready, Accepted | Todo) => true,
        // cancelled is reachable from any non-accepted active state.
        (Todo | InProgress | NeedsReview | Ready, Cancelled) => true,
        // accepted and cancelled are terminal — no outgoing edges.
        _ => false,
    }
}

/// Whether reaching this status **clears** a card as a blocker, so its
/// dependents become Available. A blocker clears only at `ready` or later, so
/// downstream work always builds on peer-reviewed work (design/project.md §4).
pub fn clears_dependents(status: Status) -> bool {
    matches!(status, Status::Ready | Status::Accepted)
}

/// A human-readable reason a transition is rejected, for the CLI error.
pub fn rejection_reason(from: Status, to: Status) -> String {
    if from.is_terminal() {
        format!(
            "{from} is terminal; a card cannot leave it (a regression becomes a new card). Use --force only for a genuine reopen."
        )
    } else {
        let allowed: Vec<&str> = Status::ALL
            .into_iter()
            .filter(|&t| t != from && transition_allowed(from, t))
            .map(|t| t.token())
            .collect();
        format!(
            "{from} -> {to} is not a legal transition. From {from} you may move to: {}.",
            allowed.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Status::*;

    #[test]
    fn every_status_is_an_idempotent_noop() {
        for st in Status::ALL {
            assert!(transition_allowed(st, st));
        }
    }

    #[test]
    fn the_happy_path_flows() {
        assert!(transition_allowed(Todo, InProgress));
        assert!(transition_allowed(InProgress, NeedsReview));
        assert!(transition_allowed(NeedsReview, Ready));
        assert!(transition_allowed(Ready, Accepted));
    }

    #[test]
    fn reviewer_and_owner_bounces_are_asymmetric() {
        // Reviewer bounce: needs-review -> in-progress (light).
        assert!(transition_allowed(NeedsReview, InProgress));
        // Owner return: ready -> todo (heavy), NOT ready -> in-progress.
        assert!(transition_allowed(Ready, Todo));
        assert!(!transition_allowed(Ready, InProgress));
    }

    #[test]
    fn abandonment_returns_to_todo() {
        assert!(transition_allowed(InProgress, Todo));
        assert!(transition_allowed(NeedsReview, Todo));
        assert!(transition_allowed(Ready, Todo));
    }

    #[test]
    fn cancelled_reachable_from_any_non_accepted() {
        for st in [Todo, InProgress, NeedsReview, Ready] {
            assert!(transition_allowed(st, Cancelled), "{st} -> cancelled");
        }
        // ...but not from accepted (terminal).
        assert!(!transition_allowed(Accepted, Cancelled));
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for &term in &[Accepted, Cancelled] {
            for to in Status::ALL {
                if to != term {
                    assert!(
                        !transition_allowed(term, to),
                        "{term} -> {to} must be rejected"
                    );
                }
            }
        }
    }

    #[test]
    fn cannot_skip_the_pipeline() {
        assert!(!transition_allowed(Todo, NeedsReview));
        assert!(!transition_allowed(Todo, Ready));
        assert!(!transition_allowed(Todo, Accepted));
        assert!(!transition_allowed(InProgress, Ready));
        assert!(!transition_allowed(NeedsReview, Accepted));
    }

    #[test]
    fn only_ready_and_accepted_clear_dependents() {
        assert!(clears_dependents(Ready));
        assert!(clears_dependents(Accepted));
        for st in [Todo, InProgress, NeedsReview, Cancelled] {
            assert!(!clears_dependents(st), "{st} must not clear dependents");
        }
    }

    #[test]
    fn rejection_reason_is_helpful() {
        let msg = rejection_reason(Todo, Accepted);
        assert!(
            msg.contains("in-progress"),
            "should list legal targets: {msg}"
        );
        let term = rejection_reason(Accepted, Todo);
        assert!(
            term.contains("terminal"),
            "should explain terminality: {term}"
        );
    }
}
