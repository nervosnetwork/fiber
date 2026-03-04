use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{error::CALL_EXECUTION_FAILED_CODE, ErrorObjectOwned};

pub use fiber_json_types::{PprofParams, PprofResult};

/// RPC module for profiling
/// This module require build with pprof feature and debug symbol.
#[rpc(server)]
trait ProfRpc {
    /// Collects a temporary CPU profile and writes a flamegraph SVG to disk.
    #[method(name = "pprof")]
    async fn pprof(&self, params: PprofParams) -> Result<PprofResult, ErrorObjectOwned>;
}

#[derive(Default)]
pub struct ProfRpcServerImpl;

impl ProfRpcServerImpl {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl ProfRpcServer for ProfRpcServerImpl {
    async fn pprof(&self, params: PprofParams) -> Result<PprofResult, ErrorObjectOwned> {
        self.pprof(params).await
    }
}

impl ProfRpcServerImpl {
    pub async fn pprof(&self, params: PprofParams) -> Result<PprofResult, ErrorObjectOwned> {
        let duration = params.duration_secs.unwrap_or(10).max(1);

        match crate::fiber::profiling::collect_flamegraph(duration).await {
            Ok(path) => Ok(PprofResult {
                path: path.to_string_lossy().into_owned(),
            }),
            Err(err) => Err(ErrorObjectOwned::owned(
                CALL_EXECUTION_FAILED_CODE,
                err.to_string(),
                Some(params),
            )),
        }
    }
}
