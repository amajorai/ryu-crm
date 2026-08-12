# ryu-crm

Harbor for Ryu — an object-first CRM that starts as a data model rather than a fixed set of screens: define your own objects and typed fields, keep records as validated value bags with relation edges materialised both ways, work them through saved table/board/list views, and read one unified per-record timeline.

> **The public home of `ryu-crm`.** Source, builds, and releases live here —
> binaries for every platform are attached to each release.
>
> This tree is generated from the Ryu monorepo, so commits pushed here
> directly are replaced on the next sync. **Pull requests are welcome** —
> open them here and they are ported into the monorepo, then flow back out.
> Ryu as a whole: https://github.com/amajorai/ryu

## Install

- Binary: `ryu-crm` from the [Ryu releases](https://github.com/amajorai/ryu/releases).
- Crate: `cargo install ryu-crm`.

## License

Apache-2.0 — see [LICENSE](./LICENSE).

---

# Harbor (`@ryu/crm`)

An object-first, schema-flexible CRM that runs entirely on your node.

Harbor starts as a **data model**, not a fixed set of screens. It ships the five
objects every pipeline needs — companies, people, deals, notes, tasks — and then
gets out of the way: add your own objects, give any of them typed fields, and every
view, filter, board and report picks them up without a migration or a settings trip.
There is no `if (object === "company")` anywhere in the panel, and that is the
product.

Everything lives in one SQLite database the node owns. No cloud tenant, no seat
count, no third party holding your pipeline.

---

## The object model

| Table | What it holds |
| --- | --- |
| `objects` | Slug, singular/plural names, icon, title field. Five are seeded as standard; the rest are yours. |
| `fields` | Typed attributes per object — 17 types, from `text` and `currency` through `status`, `multi_select`, `rating` and `relation`. `config` carries select options, relation targets, currency codes. |
| `records` | A validated JSON value bag per row, keyed by field slug. Soft-deleted, so a delete is restorable. |
| `record_links` | Relation edges, materialised in **both** directions. |
| `views` | Saved table / board / list views: filter tree, sort list, visible field ids and order, board group-by. |
| `lists` + `list_entries` | Attio-style curated subsets with **list-specific fields** — a deal's stage inside one sales list, separate from the object's own status field. |
| `activities` | The unified timeline: notes, calls, meetings, tasks, plus the `field_change` / `stage_change` entries the store writes itself. |
| `import_jobs` | A CSV import with its column mapping, dedupe key, dry-run preview and result. |

A few decisions worth knowing:

- **Money is integer cents everywhere.** It divides exactly once, at the render
  edge, through `Intl.NumberFormat` with the field's currency — so a JPY amount
  gets no minor unit rather than being rendered a hundred times too small.
- **Relations are bidirectional edges, not a foreign key.** That is why a company
  page can show its people, its open deals, and every note written against any of
  them without anyone modelling the reverse side.
- **Audit entries are written by the store, not by a caller.** No route accepts a
  `field_change`. They are read back through the timeline and nothing else.
- **Changing a `select` field's options never orphans a value.** The store either
  migrates the existing values or rejects the edit with a reason.

## CSV import

First-class in v1, because an empty CRM is unusable and this is the step everyone
defers:

1. `POST /imports?object=<slug>` with the raw CSV. The bytes are persisted **with
   the job**, so preview and apply are two requests over the same file rather than
   two uploads.
2. The job comes back with inferred columns, sampled values, and a guessed field
   per column — a tidy file needs no mapping work, only confirmation.
3. `PUT /imports/:id/mapping` sets the column→field mapping (including "do not
   import") and the dedupe key.
4. `POST /imports/:id/preview` is a **dry run**. Nothing is written. It reports
   exact create / update / skip / error counts for the whole file, plus per-row
   conflicts and the columns nobody mapped.
5. `POST /imports/:id/apply` writes. Idempotent per job — applying twice does not
   double-create.

Duplicate detection and field-by-field merge reparent activities, list memberships
and relation edges onto the survivor before soft-deleting the loser.

## Surfaces

**Desktop** — a native dock panel (`contributes.dock_panels`), registered in
`NATIVE_DOCK_PANELS` in `apps/desktop/src/components/panels/WorkspacePanels.tsx`.
It fetches this sidecar over the generic ext-proxy. That is deliberate: a
CSP-sandboxed companion would have needed per-app bridge rows in `rpc.ts` and
`kernel-contracts` — the per-app Core coupling `AGENTS.md` forbids — and could not
have imported `@ryu/ui`, which for a CRM means hand-rolling the data grid.

**Agents** — eight tools declared as manifest `runnables` of `kind: "tool"` over
the same proxy (`crm__search`, `find_record`, `get_record`, `create_record`,
`update_record`, `log_activity`, `create_task`, `pipeline`). No MCP server and no
turn hook: the plugin sandbox has no HTTP, so a hook could not reach this sidecar
at all.

**Workflows** — four `contributes.hook_events`. `deal.stage_changed` fires only on
a real transition and carries both the old and new stage, so a rule can gate on a
direction; `task.due` is claimed with a compare-and-set, so two Harbor processes
against one data dir still announce a task exactly once.

## Running it standalone

```sh
cargo run -p ryu-crm
```

Listens on `127.0.0.1:8009` (`RYU_CRM_PORT` overrides; Core injects it
profile-shifted so dev and release nodes do not collide). The database is
`crm.db` under the resolved Ryu data dir (`RYU_DIR`).

Every route under `/api/crm/*` requires the shared-secret bearer Core injects as
`RYU_EXT_TOKEN`, and the gate is **fail-closed** — with no token configured, a bare
run rejects every request rather than serving a node's contact database
unauthenticated. `/health` is the one un-gated route, because Core must probe it
before it has any reason to trust the process.

```sh
cargo test -p ryu-crm
```

## What the panel does not surface yet

The sidecar is complete; two of its surfaces have no UI in this version and are
reachable over the API only:

- **Curated lists** (`/lists/*`). The tables and routes exist, including
  list-specific fields, but the panel's rail does not render them yet.
- **Per-field merge resolution** (`MergePlan.resolutions`). The duplicates view
  shows the differing fields side by side and lets you pick which record survives,
  but the merge itself keeps the survivor's values wholesale; choosing a winner
  field by field is an API call.
- **View management** (`/objects/:object/views`, `/views/:id`). Saved views are
  read, switched between and run by the panel, and the seeded defaults cover the
  common cases — but creating, renaming, reordering or deleting one is an API call.

Everything else in this README is wired end to end: the schema editor, the grid,
the board, the record timeline, relation linking, CSV import and export, duplicate
detection and merge, the task inbox and the pipeline report.

## Deliberately out of scope for v1

Email sync, calendar sync, and Composio enrichment. Each is a different product
axis from "own your pipeline data", and each pulls in a credential story and a
polling loop that the scheduling half of the app does not need. The same call
`@ryu/social` made, for the same reason.
