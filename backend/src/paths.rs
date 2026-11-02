//! Shared Ryu sidecar data-directory seam.

use std::path::PathBuf;

/// `<data dir>/plugin-data/@ryu+crm/crm.db`.
pub fn crm_db_path() -> PathBuf {
    ryu_sidecar_runtime::plugin_data_dir("@ryu/crm").join("crm.db")
}
