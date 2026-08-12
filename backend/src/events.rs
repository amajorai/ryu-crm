//! The four app events Harbor raises, and the only place their payloads are shaped.
//!
//! ## The seam
//!
//! There is no bespoke transport here. `ryu-app-events` is the standard outbound
//! half of Ryu's hook system: it POSTs to Core's `events.emit` capability on
//! loopback using the `RYU_CORE_PORT` + `RYU_EXT_TOKEN` pair Core injects at spawn,
//! and Core fans the event out to every plugin hook whose `turn_hooks[].on` names it
//! and every workflow with a matching `event` trigger. This crate learns nothing
//! about those consumers.
//!
//! **Emitting is best-effort and never fails the write that produced it.** A record
//! that was created has been created whether or not anything was listening, so every
//! function below returns `()` and swallows transport failures (the underlying
//! `EventEmitter::emit` logs them). When the process is not Core-hosted — a
//! standalone run, this crate's own tests — every emit short-circuits to a no-op, so
//! nothing here needs a live Core.
//!
//! ## Event ids: `<plugin id>#<event name>`
//!
//! Core authorizes an emit by OWNERSHIP: the authenticated caller must *be* the
//! plugin the event id is namespaced to, and the event must appear in that plugin's
//! own `contributes.hook_events`. So the ids below are not free-form strings — each
//! one must stay **byte-identical** to an entry in
//! `apps-store/crm/manifest.json`'s `contributes.hook_events`, or Core answers the
//! emit with a 403 and the event silently never fires.
//!
//! The project brief names these events in short form; the wire ids are the
//! namespaced spellings:
//!
//! | brief shorthand           | wire id (manifest + here)      |
//! |---------------------------|--------------------------------|
//! | `crm/record.created`      | `@ryu/crm#record.created`       |
//! | `crm/record.updated`      | `@ryu/crm#record.updated`       |
//! | `crm/deal.stage_changed`  | `@ryu/crm#deal.stage_changed`   |
//! | `crm/task.due`            | `@ryu/crm#task.due`             |
//!
//! ## Why the payloads are hand-built rather than `serde_json::to_value(record)`
//!
//! A hook body is user-authored JS reading `payload.x`. Serialising the whole `Record`
//! would make every internal field addition part of a public contract nobody declared,
//! and would ship a `values` bag whose keys change whenever a user edits their schema.
//! Each payload below is therefore an explicit, documented projection: stable keys,
//! plus the raw `values` map only where a consumer genuinely needs it.

use serde_json::json;

use ryu_app_events::EventEmitter;

use crate::models::{Activity, FieldChange, Object, Record};

/// This app's manifest `id`. Core authorizes every app-event emit against it — the
/// caller must *be* the plugin the event is namespaced to — so it must stay
/// byte-identical to the `id` in `apps-store/crm/manifest.json`.
pub const PLUGIN_ID: &str = "@ryu/crm";

/// A record of any object was created. Fires once per committed insert, including
/// the rows an import applies (one event per created record, not one per import).
pub const EVENT_RECORD_CREATED: &str = "@ryu/crm#record.created";

/// A record's values changed. Fires once per committed update that actually changed
/// at least one field — a PATCH whose values match what is already stored raises
/// nothing, so a polling client cannot spam consumers. Soft delete/restore do NOT
/// raise it.
pub const EVENT_RECORD_UPDATED: &str = "@ryu/crm#record.updated";

/// A record's `status`-typed field moved between options. Fires IN ADDITION TO
/// `record.updated`, because "the deal reached Won" is the thing automations key on
/// and digging it out of a generic change list is exactly the wiring this event
/// exists to delete. Named `deal.stage_changed` after the standard object it was
/// designed for, but it is raised for a status field on ANY object.
pub const EVENT_DEAL_STAGE_CHANGED: &str = "@ryu/crm#deal.stage_changed";

/// A task activity reached its `due_at` without being completed. Raised by the
/// due-task sweep (see `store::claim_due_tasks`), at most ONCE per task: the claim
/// stamps `due_notified_at` in the same statement that selects the row, so a restart
/// mid-sweep cannot re-announce a task it already announced.
pub const EVENT_TASK_DUE: &str = "@ryu/crm#task.due";

/// Every id above, in declaration order. Exists so the manifest test (and a later
/// smoke check) can assert the manifest declares exactly this set rather than
/// eyeballing four string literals.
pub const ALL_EVENTS: [&str; 4] = [
    EVENT_RECORD_CREATED,
    EVENT_RECORD_UPDATED,
    EVENT_DEAL_STAGE_CHANGED,
    EVENT_TASK_DUE,
];

/// The shared projection every record-shaped payload starts from.
///
/// `object_slug` is included alongside `object_id` deliberately: a hook author
/// writes `if (payload.object_slug === "deal")`, and making them look up an opaque
/// id first would push every consumer into a Core round-trip this app cannot help
/// them with.
fn record_envelope(record: &Record, object: &Object) -> serde_json::Value {
    json!({
        "id": record.id,
        "object_id": record.object_id,
        "object_slug": object.slug,
        "title": record.title,
        "values": record.values,
        "created_at": record.created_at,
        "updated_at": record.updated_at,
    })
}

/// Raise [`EVENT_RECORD_CREATED`].
///
/// Call AFTER the insert commits, never before: a consumer that reacts by reading
/// the record back must not lose the race, and an event for a row that then fails to
/// commit is unrecallable.
pub async fn record_created(events: &EventEmitter, record: &Record, object: &Object) {
    events
        .emit(EVENT_RECORD_CREATED, record_envelope(record, object))
        .await;
}

/// Raise [`EVENT_RECORD_UPDATED`], carrying the per-field before/after set.
///
/// `changes` is `RecordUpdate::changed` — the store computes it by diffing the
/// normalized value bags, so it contains only fields that genuinely moved. Callers
/// must skip the emit entirely when it is empty (see the event's doc); this function
/// does not decide that for them, because the same emptiness also decides whether the
/// caller writes a `field_change` activity.
pub async fn record_updated(
    events: &EventEmitter,
    record: &Record,
    object: &Object,
    changes: &[FieldChange],
) {
    let mut payload = record_envelope(record, object);
    payload["changed"] = json!(changes
        .iter()
        .map(|c| json!({
            "field_id": c.field_id,
            "field_slug": c.field_slug,
            "field_name": c.field_name,
            "from": c.from,
            "to": c.to,
        }))
        .collect::<Vec<_>>());
    events.emit(EVENT_RECORD_UPDATED, payload).await;
}

/// Raise [`EVENT_DEAL_STAGE_CHANGED`] for one status-field transition.
///
/// `from`/`to` are the status field's OPTION IDs (stable across a rename) and
/// `from_label`/`to_label` the labels shown in the UI at the time. Both halves are
/// sent because a hook that routes on stage wants the id, and a hook that posts to
/// Slack wants the label; deriving either from the other needs the schema.
#[allow(clippy::too_many_arguments)]
pub async fn deal_stage_changed(
    events: &EventEmitter,
    record: &Record,
    object: &Object,
    field_id: &str,
    field_slug: &str,
    from: Option<&str>,
    from_label: Option<&str>,
    to: Option<&str>,
    to_label: Option<&str>,
) {
    let mut payload = record_envelope(record, object);
    payload["field_id"] = json!(field_id);
    payload["field_slug"] = json!(field_slug);
    payload["from"] = json!(from);
    payload["from_label"] = json!(from_label);
    payload["to"] = json!(to);
    payload["to_label"] = json!(to_label);
    events.emit(EVENT_DEAL_STAGE_CHANGED, payload).await;
}

/// Raise [`EVENT_TASK_DUE`] for one claimed task.
///
/// The payload carries the task's own fields plus the record it hangs off, so a hook
/// can notify the assignee without a second lookup. `record_id` is `None` for a
/// standalone task (one created against no record), which is why it is nullable here
/// rather than assumed.
pub async fn task_due(events: &EventEmitter, activity: &Activity) {
    events
        .emit(
            EVENT_TASK_DUE,
            json!({
                "id": activity.id,
                "record_id": activity.record_id,
                "object_id": activity.object_id,
                "title": activity.title,
                "body": activity.body,
                "assignee": activity.assignee,
                "due_at": activity.due_at,
                "created_at": activity.created_at,
            }),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every id must be `<PLUGIN_ID>#<name>`: Core rejects an emit whose namespace
    /// is not the calling plugin, and a typo here fails at runtime with a 403 that
    /// looks like a permissions problem rather than a string problem.
    #[test]
    fn every_event_id_is_namespaced_to_this_plugin() {
        for id in ALL_EVENTS {
            let (owner, name) = id.split_once('#').expect("event id must contain '#'");
            assert_eq!(owner, PLUGIN_ID, "{id} is namespaced to the wrong plugin");
            assert!(!name.is_empty(), "{id} has an empty event name");
            assert!(
                !name.contains('/'),
                "{id}: an event NAME must not contain '/' (that is the phase namespace)"
            );
        }
    }

    #[test]
    fn event_ids_are_unique() {
        let mut sorted = ALL_EVENTS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ALL_EVENTS.len());
    }

    /// An emitter with no Core behind it must be safe to call — this crate's own
    /// tests and any standalone run depend on it.
    #[tokio::test]
    async fn emitting_without_a_host_is_a_no_op() {
        let events = EventEmitter::from_env(PLUGIN_ID);
        let object = Object::sample();
        let record = Record::sample(&object.id);
        record_created(&events, &record, &object).await;
        record_updated(&events, &record, &object, &[]).await;
    }
}
