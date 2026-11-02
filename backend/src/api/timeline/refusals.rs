//! Refusals for timeline entries, one whole sentence per entry kind.
//!
//! The kind and the change are never spliced into a sentence as English words:
//! the Companion translates each refusal by matching the complete sentence, and
//! a spliced word could not agree with the rest of it in another language.

use super::*;

/// The change [`guard_user_authored`] is asked to allow.
#[derive(Debug, Clone, Copy)]
pub(super) enum EntryChange {
    Edit,
    Delete,
}

/// Refuse an edit or a delete on an entry the store wrote itself.
///
/// `store::update_activity` and `store::delete_activity` are kind-blind by design —
/// they are also how a record cascade removes history — so this is the only thing
/// standing between a user and a rewritten audit trail.
pub(super) fn guard_user_authored(activity: &Activity, change: EntryChange) -> ApiResult<()> {
    match (activity.kind, change) {
        (
            ActivityKind::Note | ActivityKind::Task | ActivityKind::Call | ActivityKind::Meeting,
            _,
        ) => Ok(()),
        (ActivityKind::FieldChange, EntryChange::Edit) => Err(ApiError::conflict(
            "a field change entry is written automatically and cannot be edited",
        )),
        (ActivityKind::FieldChange, EntryChange::Delete) => Err(ApiError::conflict(
            "a field change entry is written automatically and cannot be deleted",
        )),
        (ActivityKind::StageChange, EntryChange::Edit) => Err(ApiError::conflict(
            "a stage change entry is written automatically and cannot be edited",
        )),
        (ActivityKind::StageChange, EntryChange::Delete) => Err(ApiError::conflict(
            "a stage change entry is written automatically and cannot be deleted",
        )),
    }
}

/// Refuse completing an entry that is not a task; `None` for a task.
pub(super) fn not_a_task(title: &str, kind: ActivityKind) -> Option<ApiError> {
    match kind {
        ActivityKind::Task => None,
        ActivityKind::Note => Some(ApiError::conflict(format!(
            "only a task can be completed; \"{title}\" is a note entry"
        ))),
        ActivityKind::Call => Some(ApiError::conflict(format!(
            "only a task can be completed; \"{title}\" is a call entry"
        ))),
        ActivityKind::Meeting => Some(ApiError::conflict(format!(
            "only a task can be completed; \"{title}\" is a meeting entry"
        ))),
        ActivityKind::FieldChange => Some(ApiError::conflict(format!(
            "only a task can be completed; \"{title}\" is a field change entry"
        ))),
        ActivityKind::StageChange => Some(ApiError::conflict(format!(
            "only a task can be completed; \"{title}\" is a stage change entry"
        ))),
    }
}
