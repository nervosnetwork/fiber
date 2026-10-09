use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use fiber_json_types::{
    GetChannelSigningStatusParams, GetChannelSigningStatusResult, GetLspTenantRegistryNonceParams,
    GetLspTenantRegistryNonceResult, GetWatchtowerSigningStatusParams,
    GetWatchtowerSigningStatusResult, Hash256, RegisterLspTenantParams, RegisterLspTenantResult,
    SubmitChannelSignatureParams, SubmitChannelSignatureResult, SubmitWatchtowerSignatureParams,
    SubmitWatchtowerSignatureResult,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

/// Signer control-plane RPC boundary used by the independent SDK agent.
///
/// The Bruno driver uses the tenant token emitted by this agent to exercise
/// standard `new_invoice` and `send_payment` data-plane RPCs directly.
#[async_trait]
pub trait FiberRpc: Clone + Send + Sync + 'static {
    /// Release a checked preimage to fulfill an invoice.
    async fn settle_invoice(
        &self,
        _token: &str,
        _params: fiber_json_types::SettleInvoiceParams,
    ) -> Result<()> {
        anyhow::bail!("settle_invoice is not implemented by this transport")
    }

    /// Release a checked preimage to the watchtower.
    async fn create_preimage(
        &self,
        _token: &str,
        _params: fiber_json_types::CreatePreimageParams,
    ) -> Result<()> {
        anyhow::bail!("create_preimage is not implemented by this transport")
    }

    async fn open_tenant_channel(
        &self,
        _token: &str,
        _params: fiber_json_types::OpenChannelWithExternalFundingParams,
    ) -> Result<fiber_json_types::OpenTenantChannelResult> {
        anyhow::bail!("opening RPC is not implemented by this transport")
    }

    async fn get_tenant_registry_nonce(
        &self,
        root_signer_pubkey: fiber_json_types::Pubkey,
    ) -> Result<GetLspTenantRegistryNonceResult>;
    async fn register_tenant(
        &self,
        params: RegisterLspTenantParams,
    ) -> Result<RegisterLspTenantResult>;
    async fn get_channel_signing_status(
        &self,
        tenant_token: &str,
        channel_id: Hash256,
    ) -> Result<GetChannelSigningStatusResult>;
    async fn submit_channel_signature(
        &self,
        tenant_token: &str,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult>;
    async fn get_watchtower_signing_status(
        &self,
        tenant_token: &str,
        channel_id: Hash256,
    ) -> Result<GetWatchtowerSigningStatusResult>;
    async fn submit_watchtower_signature(
        &self,
        tenant_token: &str,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult>;
}

#[derive(Clone, Debug)]
pub struct HttpFiberRpc {
    url: String,
    client: reqwest::Client,
    operator_token: String,
}

impl HttpFiberRpc {
    pub fn new(url: &str, operator_token: String) -> Result<Self> {
        let url = if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("http://{url}")
        };
        Ok(Self {
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .context("build HTTP client")?,
            operator_token,
        })
    }

    async fn call<P, R>(&self, method: &str, params: &P, token: &str) -> Result<R>
    where
        P: Serialize + Sync,
        R: DeserializeOwned,
    {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": [params],
            "id": 1,
        });
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(token)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("send {method} to {}", self.url))?;
        let status = response.status();
        let body: Value = response.json().await.context("decode JSON-RPC envelope")?;
        if !status.is_success() {
            return Err(anyhow!("HTTP {status} from {}: {body}", self.url));
        }
        if let Some(error) = body.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown JSON-RPC error");
            return Err(anyhow!("{message}"));
        }
        serde_json::from_value(body.get("result").cloned().unwrap_or(Value::Null))
            .with_context(|| format!("decode JSON-RPC result for {method}"))
    }
}

#[async_trait]
impl FiberRpc for HttpFiberRpc {
    async fn settle_invoice(
        &self,
        token: &str,
        params: fiber_json_types::SettleInvoiceParams,
    ) -> Result<()> {
        let _: fiber_json_types::SettleInvoiceResult =
            self.call("settle_invoice", &params, token).await?;
        Ok(())
    }

    async fn create_preimage(
        &self,
        token: &str,
        params: fiber_json_types::CreatePreimageParams,
    ) -> Result<()> {
        self.call("create_preimage", &params, token).await
    }

    async fn open_tenant_channel(
        &self,
        token: &str,
        params: fiber_json_types::OpenChannelWithExternalFundingParams,
    ) -> Result<fiber_json_types::OpenTenantChannelResult> {
        self.call("open_tenant_channel", &params, token).await
    }

    async fn get_tenant_registry_nonce(
        &self,
        root_signer_pubkey: fiber_json_types::Pubkey,
    ) -> Result<GetLspTenantRegistryNonceResult> {
        self.call(
            "lsp_get_tenant_registry_nonce",
            &GetLspTenantRegistryNonceParams { root_signer_pubkey },
            &self.operator_token,
        )
        .await
    }

    async fn register_tenant(
        &self,
        params: RegisterLspTenantParams,
    ) -> Result<RegisterLspTenantResult> {
        self.call("lsp_register_tenant", &params, &self.operator_token)
            .await
    }

    async fn get_channel_signing_status(
        &self,
        tenant_token: &str,
        channel_id: Hash256,
    ) -> Result<GetChannelSigningStatusResult> {
        self.call(
            "get_channel_signing_status",
            &GetChannelSigningStatusParams { channel_id },
            tenant_token,
        )
        .await
    }

    async fn submit_channel_signature(
        &self,
        tenant_token: &str,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult> {
        self.call("submit_channel_signature", &params, tenant_token)
            .await
    }

    async fn get_watchtower_signing_status(
        &self,
        tenant_token: &str,
        channel_id: Hash256,
    ) -> Result<GetWatchtowerSigningStatusResult> {
        self.call(
            "get_watchtower_signing_status",
            &GetWatchtowerSigningStatusParams { channel_id },
            tenant_token,
        )
        .await
    }

    async fn submit_watchtower_signature(
        &self,
        tenant_token: &str,
        params: SubmitWatchtowerSignatureParams,
    ) -> Result<SubmitWatchtowerSignatureResult> {
        self.call("submit_watchtower_signature", &params, tenant_token)
            .await
    }
}
