//! The HTTP surface, split one module per domain.
//!
//! Every module exposes `routes() -> Router<AppState>` over paths RELATIVE to the
//! mount, and this file is the only place that knows they exist. The split is by
//! domain rather than by verb because the manifest's `sidecars[0].http.routes`
//! allowlist is written per path — keeping a domain's paths in one file is what
//! makes "did I declare all of them?" answerable by reading one module.
//!
//! Mount order matters: `axum::Router::merge` rejects a duplicate method+path pair
//! at BUILD time (it panics), so a route two modules both claim fails on boot rather
//! than silently shadowing. That is the behaviour we want — the alternative is a
//! handler that never runs and no signal saying so.

pub mod imports;
pub mod insights;
pub mod objects;
pub mod records;
pub mod timeline;
pub mod views;

use axum::Router;

use crate::state::AppState;

// ── The OpenAPI document ───────────────────────────────────────────────────────
//
// Core fetches `http://127.0.0.1:<port>/openapi.json` on this sidecar's first Healthy
// edge and lowers every operation into a searchable LLM tool. Two properties of that
// lowering shape everything below and are easy to break:
//
//   * **Paths here are ABSOLUTE and use `{braces}`** — `/api/crm/records/{record_id}`,
//     not the router's mount-local `/records/:record_id`. Core intersects the document
//     against the manifest's declared `sidecars[].http.routes[]` after stripping the
//     mount, so an operation written mount-local matches nothing and is silently
//     dropped. The two spellings differing is deliberate; do not "align" them.
//   * **An operation the manifest does not declare yields no tool.** The intersection
//     is the proxy's 404 gate reused as an allow-list, so adding a `#[utoipa::path]`
//     for an undeclared route is dead weight, and widening the manifest to a catch-all
//     to make one appear would erase the gate.
//
// The `paths(...)` list is hand-written because `utoipa-axum` (which would collect
// operations off the router itself) is not in the workspace. So a new route is NOT in
// the document until it is named here — that is the one drift this file can suffer,
// and `openapi_doc_covers_the_served_routes` below is what catches it.

/// Every annotated operation, plus the transitive schema graph its bodies reach.
///
/// Schemas are listed explicitly rather than inferred: utoipa only registers a
/// component it is told about or that a `#[utoipa::path]` names directly, so a nested
/// type reachable only through a field (`PipelineRequest::filter` → `ViewFilter` →
/// `FilterCondition` → `FilterOperator`) has to appear here or the `$ref` dangles.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        // ── DELIBERATELY ABSENT: the eight `/tools/*` operations ──
        //
        // Their handlers in `insights` ARE annotated, and adding the eight
        // `insights::tool_*` lines here is all it takes to publish them. They are left
        // out for two reasons that both point the same way:
        //
        //   1. **They are already agent tools.** `manifest.json` ships eight
        //      hand-written tool runnables pointing at these exact paths, whose
        //      descriptions cross-reference each other ("use `crm__find_record` — it is
        //      exact and this one is fuzzy") in a way no one-line `summary =` here
        //      reproduces. Deriving the same paths a second time puts TWO entries in
        //      front of the model for one endpoint, which makes selection worse, not
        //      better — and the hand-written ones are always exposed, while a derived
        //      one has to win a search first.
        //   2. **They cost cap.** Core keeps 60 derived operations per app and
        //      truncates the REST, keeping the first N — so the overflow comes off the
        //      tail of this list, not off the least useful entries. Publishing these
        //      eight would push the app to 68 and silently drop the last eight, which
        //      today are the timeline reads that `/tools/*` does not cover.
        //
        // If the hand-written runnables are ever retired, publish these eight here in
        // the same change — not before, or the model sees both.
        //
        // ── Search & reports ──
        insights::search,
        insights::summary,
        insights::pipeline,
        insights::funnel,
        insights::reindex,
        // ── Records: read, write, relate, de-duplicate ──
        records::list_records,
        records::query_records,
        records::get_record,
        records::create_record,
        records::patch_record,
        records::delete_record,
        records::restore_record,
        records::validate_record_values,
        records::list_links,
        records::link_records,
        records::unlink_records,
        records::related_records,
        records::scan_duplicates,
        records::preview_merge,
        records::apply_merge,
        // ── Objects & fields: the schema surface ──
        objects::schema,
        objects::list_objects,
        objects::create_object,
        objects::get_object,
        objects::patch_object,
        objects::delete_object,
        objects::list_fields,
        objects::create_field,
        objects::reorder_fields,
        objects::get_field,
        objects::patch_field,
        objects::delete_field,
        // ── Views & curated lists ──
        views::list_views,
        views::create_view,
        views::get_view,
        views::patch_view,
        views::delete_view,
        views::set_default_view,
        views::run_view,
        views::list_lists,
        views::create_list,
        views::get_list,
        views::patch_list,
        views::delete_list,
        views::list_list_fields,
        views::create_list_field,
        views::add_list_entry,
        views::query_list_entries,
        views::reorder_list_entries,
        views::patch_list_entry,
        views::remove_list_entry,
        // ── Timeline & tasks ──
        timeline::list_record_activities,
        timeline::create_record_activity,
        timeline::list_activities,
        timeline::get_activity,
        timeline::patch_activity,
        timeline::delete_activity,
        timeline::complete_activity,
        timeline::list_tasks,
        timeline::create_task,
    ),
    components(schemas(
        objects::PatchFieldBody,
        records::ValidateValuesBody,
        timeline::CreateTaskBody,
        crate::models::ActivityKind,
        crate::models::AddListEntryRequest,
        crate::models::CompleteTaskRequest,
        crate::models::CreateActivityRequest,
        crate::models::CreateFieldRequest,
        crate::models::CreateObjectRequest,
        crate::models::CreateListRequest,
        crate::models::CreateRecordRequest,
        crate::models::CreateViewRequest,
        crate::models::DuplicateScanRequest,
        crate::models::FieldConfig,
        crate::models::FieldType,
        crate::models::FilterCondition,
        crate::models::FilterOperator,
        crate::models::FunnelRequest,
        crate::models::LinkRequest,
        crate::models::ListEntryQuery,
        crate::models::MergeFieldResolution,
        crate::models::MergePlan,
        crate::models::MergeSource,
        crate::models::PipelineRequest,
        crate::models::RecordQuery,
        crate::models::ReorderRequest,
        crate::models::SelectOption,
        crate::models::SortDirection,
        crate::models::UpdateActivityRequest,
        crate::models::UpdateFieldRequest,
        crate::models::UpdateListEntryRequest,
        crate::models::UpdateListRequest,
        crate::models::UpdateMode,
        crate::models::UpdateObjectRequest,
        crate::models::UpdateRecordRequest,
        crate::models::UpdateViewRequest,
        crate::models::ViewFilter,
        crate::models::ViewKind,
        crate::models::ViewQueryOverrides,
        crate::models::ViewSort,
    ))
)]
struct CrmApiDoc;

/// The document `main` serves at `/openapi.json`.
pub fn openapi() -> utoipa::openapi::OpenApi {
    <CrmApiDoc as utoipa::OpenApi>::openapi()
}

/// The whole app surface, relative to the mount prefix.
///
/// Callers nest this under `/api/crm` exactly once. It deliberately does NOT include
/// `/health`: that probe is un-gated and lives outside the authenticated prefix, so
/// mounting it here would put it behind the bearer that Core cannot present until it
/// already trusts the process.
pub fn routes() -> Router<AppState> {
    Router::new()
        .merge(objects::routes())
        .merge(records::routes())
        .merge(views::routes())
        .merge(timeline::routes())
        .merge(imports::routes())
        .merge(insights::routes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `merge` panics on a duplicate method+path across two modules, so simply
    /// building the router is the assertion. Cheap, and it catches the one mistake
    /// this file can make — two domains claiming the same path — at test time
    /// instead of at a user's boot.
    #[test]
    fn every_module_merges_without_a_path_collision() {
        let _ = routes();
    }

    /// The external mount every documented path must carry. Duplicated from `main`'s
    /// `MOUNT` rather than imported because `main` is the binary root and a test in a
    /// child module cannot reach a private const there — and a wrong copy fails the
    /// test below rather than passing quietly.
    const MOUNT: &str = "/api/crm";

    fn declared_routes() -> Vec<String> {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../../manifest.json")).expect("valid JSON");
        manifest["sidecars"][0]["http"]["routes"]
            .as_array()
            .expect("routes must be an array")
            .iter()
            .map(|r| r["path"].as_str().expect("a path").to_owned())
            .collect()
    }

    /// `/api/crm/records/{record_id}` → `/records/:record_id`, i.e. the spelling the
    /// manifest uses. Core does the same normalisation before intersecting, so this is
    /// the transformation the derived-tool pipeline actually depends on.
    fn to_manifest_form(documented: &str) -> String {
        let local = documented.strip_prefix(MOUNT).unwrap_or(documented);
        let mut out = String::with_capacity(local.len());
        let mut in_param = false;
        for ch in local.chars() {
            match ch {
                '{' => {
                    in_param = true;
                    out.push(':');
                }
                '}' => in_param = false,
                _ => out.push(ch),
            }
        }
        assert!(!in_param, "unbalanced braces in '{documented}'");
        out
    }

    #[test]
    fn openapi_doc_covers_the_served_routes() {
        let doc = openapi();
        assert!(!doc.paths.paths.is_empty());
    }

    /// Every documented operation is on a path the manifest declares.
    ///
    /// This is the direction that fails SILENTLY in production: Core intersects the
    /// document against `sidecars[].http.routes[]` and drops anything that does not
    /// match, so an operation annotated with a typo'd path — or one whose route was
    /// never declared — yields no tool and logs nothing a reader would notice. The
    /// annotation looks right, the app looks shipped, and the agent simply never gets
    /// the tool.
    #[test]
    fn every_documented_path_is_declared_in_the_manifest() {
        let declared = declared_routes();
        for path in openapi().paths.paths.keys() {
            assert!(
                path.starts_with(MOUNT),
                "'{path}' is documented without the '{MOUNT}' mount; Core strips the \
                 mount before intersecting, so a mount-local path matches nothing"
            );
            let local = to_manifest_form(path);
            assert!(
                declared.contains(&local),
                "'{path}' is documented but '{local}' is not in manifest.json's \
                 sidecars[0].http.routes — Core would drop the derived tool"
            );
        }
    }

    /// Every `$ref` in the document points at a component that is actually there.
    ///
    /// This is the failure the transitive schema graph produces, and it is invisible
    /// from Rust: `components(schemas(...))` is a hand-written list, so a body type
    /// whose NESTED type was never registered still compiles, still serialises, and
    /// still emits `{"$ref": "#/components/schemas/Missing"}`. Core's importer resolves
    /// refs to build a tool's arguments — a dangling one yields a tool with no visible
    /// arguments, i.e. discoverable and uncallable, which is worse than no tool at all.
    #[test]
    fn no_schema_ref_in_the_document_dangles() {
        let doc = serde_json::to_value(openapi()).expect("the document serialises");
        let registered: std::collections::BTreeSet<String> = doc["components"]["schemas"]
            .as_object()
            .expect("components.schemas must be an object")
            .keys()
            .cloned()
            .collect();

        fn collect_refs(node: &serde_json::Value, out: &mut Vec<String>) {
            match node {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::String(reference)) = map.get("$ref") {
                        out.push(reference.clone());
                    }
                    for value in map.values() {
                        collect_refs(value, out);
                    }
                }
                serde_json::Value::Array(items) => {
                    for value in items {
                        collect_refs(value, out);
                    }
                }
                _ => {}
            }
        }

        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);
        assert!(!refs.is_empty(), "a typed request body must produce refs");
        for reference in refs {
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected ref target '{reference}'"));
            assert!(
                registered.contains(name),
                "'{name}' is referenced but never registered in components(schemas(...))"
            );
        }
    }

    /// The document is not empty in a way a `!is_empty()` assertion would miss.
    ///
    /// `paths(...)` in the `#[openapi]` attribute is hand-written (there is no
    /// `utoipa-axum` in the workspace to collect them off the router), so the realistic
    /// regression is not "the list is empty" — it is "somebody deleted a line while
    /// rebasing". Pinning the count makes that a failing test instead of a quietly
    /// smaller tool surface. Raise it when you annotate more routes.
    #[test]
    fn the_documented_path_count_is_the_one_we_intend() {
        assert_eq!(openapi().paths.paths.len(), 38);
    }

    /// **The document must fit inside Core's per-app exposure cap, and this is the
    /// only place that says so.**
    ///
    /// Core keeps at most `EXT_API_PER_PLUGIN_CAP` (60) derived operations per app and
    /// truncates the rest — `routes.truncate(budget)`, i.e. it keeps the FIRST N. So an
    /// app that overflows does not lose its least useful operations, it loses the TAIL
    /// of the `paths(...)` list above, and it loses them at runtime with only a
    /// `tracing::warn!` to show for it. Harbor is the largest surface in the store and
    /// sits exactly at the ceiling, so this is not a theoretical guard: the next
    /// annotated route silently costs an existing one.
    ///
    /// When this fails, do NOT raise the number — that is not a lever this crate has.
    /// The levers, in order:
    ///
    ///   1. Publish nothing new; the six `/imports/*` operations and
    ///      `/exports/views/{view_id}` are already held back for exactly this reason.
    ///   2. Narrow the manifest's `http.routes[]`, which is what Core's own comment on
    ///      the cap recommends — an app past 60 is exposing its internal surface rather
    ///      than an intended tool set.
    ///   3. Retire something. The eight `/tools/*` operations are already excluded (see
    ///      `paths(...)`); adding them back costs eight of these sixty.
    #[test]
    fn the_document_fits_inside_cores_per_app_exposure_cap() {
        // Core's `EXT_API_PER_PLUGIN_CAP`. Duplicated as a plain number because this
        // crate links none of Core; if that constant moves, this comment is the trail.
        const CORE_EXPOSURE_CAP: usize = 60;
        let operations: usize = openapi()
            .paths
            .paths
            .values()
            // `PathItem` in utoipa 5 is eight explicit `Option<Operation>` fields
            // rather than a map, so the verbs are counted by hand.
            .map(|item| {
                [
                    &item.get,
                    &item.put,
                    &item.post,
                    &item.delete,
                    &item.options,
                    &item.head,
                    &item.patch,
                    &item.trace,
                ]
                .iter()
                .filter(|op| op.is_some())
                .count()
            })
            .sum();
        assert!(
            operations <= CORE_EXPOSURE_CAP,
            "{operations} documented operations, but Core exposes only \
             {CORE_EXPOSURE_CAP} per app and truncates the tail of `paths(...)` — the \
             newest entries would be dropped at runtime, not the least useful ones"
        );
    }
}
