//! Shared Ryu sidecar data-directory seam.

use std::path::PathBuf;

pub fn crm_db_path() -> PathBuf {
    ryu_sidecar_runtime::ryu_dir().join("crm.db")
}
