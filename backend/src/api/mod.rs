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
}
