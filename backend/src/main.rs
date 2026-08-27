//! `ryu-crm` — the standalone, out-of-process Harbor sidecar.
//!
//! An object-first, schema-flexible CRM that runs as a SEPARATE PROCESS Core spawns,
//! health-checks, and proxies to on loopback — the same shape as `ryu-social` /
//! `ryu-mail`. Core does NOT contain this code and does not link it: there is no
//! `lib.rs`, every module below is bin-private, and the only route into this process
//! is the generic ext-proxy. So Harbor scales, fails, and ships independently of the
//! rest of the node.
//!
//! Contract surface — the paths Core forwards to, byte-identical whether they arrive
//! via the `public_mount` (`/api/crm/*`) or the plugin proxy
//! (`/api/ext/@ryu/crm/*`, rewritten onto the same prefix):
//!
//! ```text
//!   /health                       — un-gated loopback probe
//!   /api/crm/*                    — the whole app surface (see `api::routes`)
//! ```
//!
//! SECURITY: this binary binds LOOPBACK ONLY (127.0.0.1) **and** guards every
//! `/api/crm/*` route with a shared-secret bearer (`RYU_EXT_TOKEN`, injected by Core
//! into this child's spawn env). Core stays the auth front — it runs its own
//! `require_auth`, then re-stamps `Authorization: Bearer <RYU_EXT_TOKEN>` on the
//! loopback hop — so a request that did NOT come through Core (any other local
//! process on a shared host) is rejected with 401. The gate is FAIL-CLOSED: with no
//! token configured, every protected route rejects rather than falling open. That
//! matters more here than in most sidecars: a CRM database is the single most
//! sensitive blob on a sales node, and "no token" is exactly the state a bare
//! `./ryu-crm` run is in.
//!
//! `/health` is the ONE un-gated route. It has to be: Core probes it BEFORE it has
//! any reason to trust this process, and it returns counts only, never record content.
//!
//! Port: `RYU_CRM_PORT` env, default `state::DEFAULT_PORT`. Data dir: resolved via
//! `paths::ryu_dir` (`RYU_DIR`-env-first, injected by Core at spawn), so it opens the
//! SAME `crm.db` the node uses. This sidecar OWNS that database; nothing else opens it.

// The domain contract and the persistence layer are written to be complete for the
// modules that land beside them, so plenty of their surface has no caller YET.
// Scoped to those modules rather than a crate-wide blanket, so real dead code in the
// handlers still warns.
#[allow(dead_code)]
mod models;
#[allow(dead_code)]
mod store;

// `events` exposes one emit helper per declared hook event ahead of every call site
// landing, and `state` carries the limits + emitter the router modules consume.
#[allow(dead_code)]
mod events;
#[allow(dead_code)]
mod state;

mod api;
mod error;
mod paths;

use std::net::{Ipv4Addr, SocketAddr};

use axum::{
    extract::Request,
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{from_fn, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::json;

use crate::state::{AppState, Config, DEFAULT_PORT};
use crate::store::CrmStore;

/// The external prefix. Must match the manifest's `sidecars[0].http.mount`, or Core
/// forwards `/api/crm/records` to a router that only knows `/records`.
const MOUNT: &str = "/api/crm";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let port: u16 = std::env::var("RYU_CRM_PORT")
        .ok()
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT);

    // The shared secret Core injects via the generic ext-proxy loader: a per-plugin
    // minted token it stamps on every proxied hop and on the health probe.
    let token = std::env::var("RYU_EXT_TOKEN")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if token.is_some() {
        tracing::info!(
            "ryu-crm: protected {MOUNT}/* routes require the injected shared-secret bearer"
        );
    } else {
        tracing::warn!(
            "ryu-crm: no RYU_EXT_TOKEN set; protected {MOUNT}/* routes are FAIL-CLOSED (reject all). Core injects this token when it spawns the sidecar."
        );
    }

    // Opening the store runs the migrations and seeds the five standard objects with
    // their default fields and views on a fresh database. Synchronous and fallible on
    // purpose: a CRM that cannot open its database must not come up half-alive and
    // report green.
    let store = CrmStore::open(paths::crm_db_path())?;
    let state = AppState::new(store.clone(), Config::from_env(port));

    // The due-task sweep owns its own clone of the state and runs for the process
    // lifetime, claiming overdue tasks and raising `task.due`. Spawned, not awaited:
    // `main` must keep serving HTTP. `claim_due_tasks` is a compare-and-set, so even
    // two Harbor processes against one data dir cannot double-announce a task — the
    // `task_sweep_enabled` flag is about not paying for the scan twice, not safety.
    let sweeper = spawn_task_sweep(state.clone());

    // The app router, with the shared-secret gate layered over the WHOLE nest.
    // Harbor has no public route: there is no inbound webhook here, so every path
    // under the mount is protected without exception.
    //
    // `/openapi.json` rides INSIDE that same gate, at the SERVER ROOT. Core fetches
    // `http://127.0.0.1:<port>/openapi.json` on this sidecar's first Healthy edge and
    // lowers every operation it finds into a searchable LLM tool, so routing this one
    // endpoint is what makes Harbor's surface reachable by an agent at all — without
    // it the app contributes zero derived tools no matter how many routes it serves.
    //
    // Root, not under `/api/crm`: Core tries the root FIRST and only falls back to the
    // mount-prefixed form, and a 404 on the first try is classified DEFINITIVE — it
    // would latch this app at zero derived tools for the life of the process. Keeping
    // the document off the mount also keeps it out of the manifest's declared
    // `http.routes[]`, which is right: the schema is Core's to read, not an app surface
    // the ext-proxy should forward.
    //
    // Inside the gate, not beside the un-gated `/health`: Core stamps the injected
    // `RYU_EXT_TOKEN` on the fetch, so the gate costs the fetcher nothing — while
    // un-gated it would disclose this app's entire internal API surface, including
    // every CRM write route, to any other process on loopback.
    let gated_token = token.clone();
    let app_routes = Router::new()
        .nest(MOUNT, api::routes().with_state(state))
        .route("/openapi.json", get(|| async { Json(api::openapi()) }))
        .layer(from_fn(move |req: Request, next: Next| {
            let expected = gated_token.clone();
            async move { require_crm_token(req, next, expected.as_deref()).await }
        }));

    // `/health` sits OUTSIDE the gated nest so Core's loopback probe succeeds before
    // auth. It asserts the DB is READABLE (not merely that the process is alive) and
    // returns no user data — a health check that only proved liveness would report
    // green on a node whose database is missing.
    let health_store = store;
    let app = Router::new()
        .route(
            "/health",
            get(move || {
                let store = health_store.clone();
                async move { health(store).await }
            }),
        )
        .merge(app_routes);

    // LOOPBACK ONLY (belt) + shared-secret bearer (suspenders).
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ryu-crm sidecar listening on http://{addr}{MOUNT}");

    let result = axum::serve(listener, app).await;
    // Stop the sweep on shutdown so a supervised restart does not briefly run two
    // sweeps against one database.
    sweeper.abort();
    result?;
    Ok(())
}

/// The overdue-task loop: claim a bounded batch, raise `task.due` for each, sleep.
///
/// Bounded per tick (`task_sweep_batch`) so a node that was offline for a month
/// emits its backlog over several ticks instead of one thundering burst, and gated by
/// `task_sweep_enabled` so a test harness or a read-only second process can turn it
/// off entirely.
fn spawn_task_sweep(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !state.config.task_sweep_enabled {
            tracing::info!("ryu-crm: due-task sweep disabled (RYU_CRM_TASK_SWEEP)");
            return;
        }
        let interval = std::time::Duration::from_secs(state.config.task_sweep_interval_secs);
        loop {
            tokio::time::sleep(interval).await;
            match state
                .store
                .claim_due_tasks(state.config.task_sweep_batch)
                .await
            {
                Ok(due) => {
                    for task in due {
                        events::task_due(&state.events, &task).await;
                    }
                }
                // Best-effort: a transient read error must not kill the loop for the
                // life of the process.
                Err(e) => tracing::warn!(error = %e, "ryu-crm: due-task sweep failed"),
            }
        }
    })
}

/// Un-gated loopback health probe. Confirms DB readiness with a cheap read and
/// returns counts only — never record content.
async fn health(store: CrmStore) -> Response {
    match store.list_objects().await {
        Ok(objects) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "objectCount": objects.len() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Shared-secret bearer gate for the proxied surface.
///
/// **Fail-closed:** `expected == None`/empty (no token configured) rejects every
/// request rather than falling open, so a bare-run or misconfigured sidecar never
/// serves a node's contact database unauthenticated.
async fn require_crm_token(req: Request, next: Next, expected: Option<&str>) -> Response {
    let provided = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if bearer_ok(provided, expected) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

/// Pure bearer check, factored out so the auth decision is unit-testable without an
/// axum `Request`/`Next`. Returns `true` only when `expected` is a non-empty token
/// AND `provided` equals it (constant-time compared).
fn bearer_ok(provided: Option<&str>, expected: Option<&str>) -> bool {
	ryu_sidecar_runtime::token_ok(provided, expected)
}


#[cfg(test)]
mod tests {
    use super::bearer_ok;

    #[test]
    fn bearer_ok_matches_only_exact_nonempty_token() {
        assert!(bearer_ok(Some("secret"), Some("secret")));
        assert!(!bearer_ok(Some("secret"), Some("other")));
        assert!(!bearer_ok(Some("secre"), Some("secret")));
        assert!(!bearer_ok(None, Some("secret")));
    }

    #[test]
    fn bearer_ok_is_fail_closed_without_expected() {
        // No/empty configured token → reject everything, even a matching-looking hdr.
        assert!(!bearer_ok(Some("secret"), None));
        assert!(!bearer_ok(Some(""), Some("")));
        assert!(!bearer_ok(None, None));
    }
}
