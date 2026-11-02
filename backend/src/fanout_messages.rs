//! Inbox, toast and push copy as localizable messages.
//!
//! Core renders a notify hint's `i18n` text per recipient (the inbox row and
//! toast in the node's `ui-language`, each push in the locale its device
//! registered with); the English rendering still goes out as the plain
//! `title`/`body`, which an older Core delivers as is.
//!
//! Each `*_MESSAGE` is `format!` with one named argument per placeholder
//! (`name = arg("name")`) so the i18n extractor catalogs the ICU source;
//! `tools/generate-core-i18n.mjs` copies the translations into Core's catalog.
//! Every message is a whole sentence. Core's renderer has no plural selector,
//! so counts are written as `label: {count}`.
#![allow(clippy::useless_format)]

use ryu_app_events::{NotifyHint, NotifyMessage, NotifyTemplate as Template};

pub(crate) const DEAL_STAGE_CHANGED_MESSAGE: Template =
    |arg| format!("A deal moved to {stage}", stage = arg("stage"));
pub(crate) const FOLLOW_UP_DUE_MESSAGE: Template = |_| format!("A follow-up is due");

/// The Inbox notice for a deal stage move. It names only the stage label (field
/// schema, not record content); `records.title` is sealed.
pub(crate) fn stage_changed(stage_label: &str) -> NotifyHint {
    NotifyHint::localized(
        NotifyMessage::new(DEAL_STAGE_CHANGED_MESSAGE).text("stage", stage_label),
        Vec::new(),
    )
}

/// The Inbox notice for a task coming due. Generic: the task text is sealed.
pub(crate) fn task_due() -> NotifyHint {
    NotifyHint::localized(NotifyMessage::new(FOLLOW_UP_DUE_MESSAGE), Vec::new())
}
