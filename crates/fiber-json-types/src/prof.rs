//! Profiling types for the Fiber Network JSON-RPC API.

use crate::serde_utils::U64Hex;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

/// Parameters for profiling.
#[serde_as]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PprofParams {
    /// Duration to profile in seconds. Defaults 10s.
    #[serde_as(as = "Option<U64Hex>")]
    pub duration_secs: Option<u64>,
}

/// Result of profiling.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PprofResult {
    /// Path of the generated flamegraph SVG.
    pub path: String,
}
