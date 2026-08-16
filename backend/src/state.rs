//! The axum state every handler is built over: the store, the process config, and
//! the app-event emitter.
//!
//! One state struct rather than per-module states, because the six later-owned
//! router modules (objects, records, views, timeline, imports, insights) each need
//! at least two of these three, and a narrower state per module would just mean
//! converting between them at every call. Every field is cheap to clone (`Arc`
//! inside), so `State<AppState>` extraction costs nothing per request.

use std::sync::Arc;

use crate::events::PLUGIN_ID;
use crate::store::CrmStore;

/// Default page size when a query names none. Small enough that the panel's first
/// paint is one screen of rows, large enough that a board view with five columns is
/// not five round-trips.
pub const DEFAULT_PAGE_SIZE: usize = 50;

/// Hard ceiling on `limit`.
///
/// **Applied by the HANDLER, not by the store.** Every paginated store function takes
/// `limit`/`offset` as separate arguments and uses them verbatim; the `limit`/`offset`
/// fields on the query structs (`RecordQuery`, `ActivityQuery`, …) are WIRE-ONLY and
/// the store ignores them. So a handler must always write
/// `state.config.clamp_limit(body.limit)` and pass the result — deserializing a body
/// and forwarding `body.limit` straight through compiles, looks right, and has no
/// ceiling at all. `records.data` is an unbounded JSON blob per row, so "let them ask
/// for everything" is a memory bug waiting for one big tenant.
pub const MAX_PAGE_SIZE: usize = 500;

/// Ceiling on ONE uploaded CSV, in bytes. The raw text is persisted in the
/// `import_jobs` row so preview and apply are separate requests over the same
/// bytes — which means an unbounded upload is an unbounded row in the database, not
/// merely a large request. 16 MiB is ~150k typical CRM rows.
pub const MAX_IMPORT_BYTES: usize = 16 * 1024 * 1024;

/// How often the due-task sweep runs when it is enabled.
pub const TASK_SWEEP_INTERVAL_SECS: u64 = 60;

/// How many overdue tasks one sweep may claim. Bounds the blast radius of a backlog:
/// without it, a node that was offline for a month would emit its entire overdue
/// list in one tick.
pub const TASK_SWEEP_BATCH: usize = 100;

/// Process-level configuration, resolved once at boot from the environment.
///
/// Deliberately distinct from anything a user can edit: nothing in this struct is
/// reachable from a settings tab, because a user must not be able to change the port
/// or lift the page ceiling from the UI.
#[derive(Debug, Clone)]
pub struct Config {
    /// The loopback port this process listens on.
    pub port: u16,
    /// Page size applied when a query names none.
    pub default_page_size: usize,
    /// Ceiling applied to every `limit`, however large the caller asked.
    pub max_page_size: usize,
    /// Ceiling on one uploaded CSV.
    pub max_import_bytes: usize,
    /// Whether the due-task sweep loop runs at all. `RYU_CRM_TASK_SWEEP=0` disables
    /// it, which is what a test harness or a second read-only replica wants — and
    /// what stops two Harbor processes against one data dir from both announcing the
    /// same task. (The claim is a CAS, so a duplicate is not *possible*; the flag is
    /// about not paying for the scan twice.)
    pub task_sweep_enabled: bool,
    /// Seconds between sweeps.
    pub task_sweep_interval_secs: u64,
    /// Tasks one sweep may claim.
    pub task_sweep_batch: usize,
}

impl Config {
    /// Read from the environment, with the defaults a normal Core-spawned run uses.
    pub fn from_env(port: u16) -> Self {
        Self {
            port,
            default_page_size: DEFAULT_PAGE_SIZE,
            max_page_size: MAX_PAGE_SIZE,
            max_import_bytes: env_usize("RYU_CRM_MAX_IMPORT_BYTES", MAX_IMPORT_BYTES),
            task_sweep_enabled: std::env::var("RYU_CRM_TASK_SWEEP")
                .map(|v| !matches!(v.trim(), "0" | "false" | "off"))
                .unwrap_or(true),
            task_sweep_interval_secs: std::env::var("RYU_CRM_TASK_SWEEP_SECS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(TASK_SWEEP_INTERVAL_SECS),
            task_sweep_batch: env_usize("RYU_CRM_TASK_SWEEP_BATCH", TASK_SWEEP_BATCH),
        }
    }

    /// Clamp a caller-supplied page size into `1..=max_page_size`, defaulting when
    /// absent. The ONE place that decision is made, so a handler that forgets to
    /// clamp cannot exist.
    pub fn clamp_limit(&self, requested: Option<usize>) -> usize {
        requested
            .filter(|n| *n > 0)
            .unwrap_or(self.default_page_size)
            .min(self.max_page_size)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env(crate::state::DEFAULT_PORT)
    }
}

/// Default loopback port for the CRM sidecar (overridable via `RYU_CRM_PORT`, which
/// Core injects profile-shifted so concurrent dev/release nodes do not collide).
///
/// Must stay equal to `sidecars[0].port` in `apps-store/crm/manifest.json`: the
/// manifest value is what Core injects and what its health probe polls, and this
/// constant is only the standalone-run fallback — a drift between the two is a
/// sidecar Core reports unhealthy while it happily serves on another port. There is
/// no port registry, so avoiding a collision is this file's job. 7990–8008 were all
/// taken by the time Harbor landed: `@ryu/reasoning` holds 8006, THREE concurrently
/// built apps (`blueprint`, `mission-control`, `tuition`) all claimed 8007, and
/// `@ryu/news` holds 8008. Harbor takes the first free one above them.
pub const DEFAULT_PORT: u16 = 8009;

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

#[derive(Clone)]
pub struct AppState {
    pub store: CrmStore,
    pub config: Arc<Config>,
    /// Raises this app's declared hook events so plugin hooks and event-triggered
    /// workflows can react to a record change without either side knowing the other
    /// exists.
    ///
    /// Safe to hold unconditionally: `from_env` never fails, and every emit no-ops
    /// when `RYU_CORE_PORT`/`RYU_EXT_TOKEN` are absent — which is the state under
    /// this crate's own tests and any standalone run, so no test needs a live Core.
    pub events: ryu_app_events::EventEmitter,
}

impl AppState {
    pub fn new(store: CrmStore, config: Config) -> Self {
        Self {
            store,
            config: Arc::new(config),
            events: ryu_app_events::EventEmitter::from_env(PLUGIN_ID),
        }
    }

    /// An in-memory state for module tests: a fresh seeded database, default config,
    /// and an unhosted emitter. A plain `pub fn`, not `#[cfg(test)]`, so each
    /// later-owned router module can build one without re-deriving the wiring.
    pub fn in_memory() -> anyhow::Result<Self> {
        Ok(Self::new(CrmStore::open_in_memory()?, Config::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_are_clamped_not_trusted() {
        let config = Config::from_env(DEFAULT_PORT);
        assert_eq!(config.clamp_limit(None), DEFAULT_PAGE_SIZE);
        assert_eq!(config.clamp_limit(Some(0)), DEFAULT_PAGE_SIZE);
        assert_eq!(config.clamp_limit(Some(10)), 10);
        assert_eq!(config.clamp_limit(Some(100_000)), MAX_PAGE_SIZE);
    }
}
