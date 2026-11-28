use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use biscuit_auth::{
    builder::{Fact, Term},
    Authorizer, AuthorizerBuilder, AuthorizerLimits, Biscuit, PublicKey,
};

use crate::now_timestamp_as_millis_u64;
use fiber_types::NodeId;
use fiber_types::TenantId;

#[cfg(feature = "lsp")]
pub use crate::lsp::BiscuitTokenIssuer;

const DEFAULT_BISCUIT_AUTH_MAX_TIME: Duration = Duration::from_millis(10);

fn authorizer_builder() -> AuthorizerBuilder {
    AuthorizerBuilder::new().set_limits(AuthorizerLimits {
        max_time: DEFAULT_BISCUIT_AUTH_MAX_TIME,
        ..Default::default()
    })
}

fn build_authorizer(token: &Biscuit) -> Result<Authorizer> {
    Ok(authorizer_builder().build(token)?)
}

pub struct AuthRule {
    pub(crate) code: &'static str,
    pub(crate) require_rpc_context: bool,
}

impl AuthRule {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            require_rpc_context: false,
        }
    }

    fn with_require_rpc_context(mut self, require_rpc_context: bool) -> Self {
        self.require_rpc_context = require_rpc_context;
        self
    }

    /// build rule
    fn build_rule(&self) -> Result<AuthorizerBuilder> {
        let authorizer = authorizer_builder()
            .code(self.code)
            .context("build authorizer code")?;
        Ok(authorizer)
    }

    /// authorize
    ///
    /// - token biscuit token
    /// - time_in_ms time in milliseconds since UNIX_EPOCH
    fn authorize(&self, token: &Biscuit, time_in_ms: u64) -> Result<()> {
        self.build_rule()?
            .fact(Fact::new(
                "time".to_string(),
                &[Term::Date(time_in_ms / 1000)],
            ))?
            .build(token)?
            .authorize()?;
        Ok(())
    }
}

#[derive(Default)]
struct RuleBuilder(HashMap<&'static str, AuthRule>);

impl RuleBuilder {
    fn new() -> Self {
        Self::default()
    }

    fn rule(&mut self, method: &'static str, code: &'static str) -> &mut Self {
        self.0.insert(method, AuthRule::new(code));
        self
    }

    fn with_rule(&mut self, method: &'static str, rule: AuthRule) -> &mut Self {
        self.0.insert(method, rule);
        self
    }
}

fn build_rules() -> HashMap<&'static str, AuthRule> {
    let mut b = RuleBuilder::new();

    // Cch
    b.rule("send_btc", r#"allow if write("cch");"#);
    b.rule("receive_btc", r#"allow if write("cch");"#);
    b.rule("get_cch_order", r#"allow if read("cch");"#);
    b.rule(
        "subscribe_store_changes",
        r#"allow if internal("store_changes");"#,
    );
    // subscriptions
    b.rule("subscribe", r#"allow if write("subscriptions");"#);
    b.rule("unsubscribe", r#"allow if write("subscriptions");"#);
    // channels
    b.rule("open_channel", r#"allow if write("channels");"#);
    b.rule("accept_channel", r#"allow if write("channels");"#);
    b.rule("abandon_channel", r#"allow if write("channels");"#);
    b.rule("list_channels", r#"allow if read("channels");"#);
    b.rule("shutdown_channel", r#"allow if write("channels");"#);
    b.rule("update_channel", r#"allow if write("channels");"#);
    b.rule(
        "open_channel_with_external_funding",
        r#"allow if write("channels");"#,
    );
    b.rule("submit_signed_funding_tx", r#"allow if write("channels");"#);
    b.rule(
        "get_channel_signing_status",
        r#"allow if read("channels");"#,
    );
    b.rule("submit_channel_signature", r#"allow if write("channels");"#);
    // dev
    b.rule("commitment_signed", r#"allow if write("dev");"#);
    b.rule("add_tlc", r#"allow if write("dev");"#);
    b.rule("remove_tlc", r#"allow if write("dev");"#);
    b.rule("check_channel_shutdown", r#"allow if write("dev");"#);
    b.rule("sign_external_funding_tx", r#"allow if write("dev");"#);
    b.rule("submit_commitment_transaction", r#"allow if write("dev");"#);
    // prof
    b.rule("pprof", r#"allow if write("pprof");"#);
    // graph
    b.rule("graph_nodes", r#"allow if read("graph");"#);
    b.rule("graph_channels", r#"allow if read("graph");"#);

    // info
    b.rule("node_info", r#"allow if read("node");"#);
    b.rule("new_invoice", r#"allow if write("invoices");"#);
    b.rule("parse_invoice", r#"allow if read("invoices");"#);
    b.rule("get_invoice", r#"allow if read("invoices");"#);
    b.rule("cancel_invoice", r#"allow if write("invoices");"#);
    b.rule("settle_invoice", r#"allow if write("invoices");"#);
    b.rule("backup", r#"allow if write("node");"#);

    // hosted LSP
    b.rule("lsp_get_status", r#"allow if read("lsp");"#);
    b.rule("lsp_register_tenant", r#"allow if write("lsp");"#);
    b.rule("lsp_ensure_tenant", r#"allow if write("lsp");"#);
    b.rule("lsp_evict_tenant", r#"allow if write("lsp");"#);
    b.rule("lsp_revoke_tenant_tokens", r#"allow if write("lsp");"#);
    b.rule("lsp_list_tenants", r#"allow if read("lsp");"#);
    b.rule("lsp_get_payment_delivery", r#"allow if read("lsp");"#);

    // payment
    b.rule("send_payment", r#"allow if write("payments");"#);
    b.rule("get_payment", r#"allow if read("payments");"#);
    b.rule("list_payments", r#"allow if read("payments");"#);
    b.rule("build_router", r#"allow if read("payments");"#);
    b.rule("send_payment_with_router", r#"allow if write("payments");"#);

    // peer
    b.rule("connect_peer", r#"allow if write("peers");"#);
    b.rule("disconnect_peer", r#"allow if write("peers");"#);
    b.rule("list_peers", r#"allow if read("peers");"#);

    // watchtower
    b.with_rule(
        "create_watch_channel",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "remove_watch_channel",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "update_revocation",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "update_pending_remote_settlement",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "update_local_settlement",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "create_preimage",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "remove_preimage",
        AuthRule::new(r#"allow if write("watchtower");"#).with_require_rpc_context(true),
    );
    b.with_rule(
        "get_watchtower_signing_status",
        AuthRule::new(
            r#"allow if read("watchtower"); allow if write("watchtower"); allow if read("channels");"#,
        )
        .with_require_rpc_context(true),
    );
    b.with_rule(
        "submit_watchtower_signature",
        AuthRule::new(r#"allow if write("watchtower"); allow if write("channels");"#)
            .with_require_rpc_context(true),
    );

    b.0
}

/// Shared runtime map of each hosted tenant's minimum valid token epoch (`> 0`),
/// paired with a `watch` notification channel so active WebSocket subscriptions
/// terminate immediately on `lsp_revoke_tenant_tokens`.
#[derive(Clone, Debug)]
pub struct TenantTokenEpochs {
    epochs: std::sync::Arc<parking_lot::RwLock<HashMap<TenantId, u64>>>,
    revocation_tx: std::sync::Arc<tokio::sync::watch::Sender<u64>>,
}

impl Default for TenantTokenEpochs {
    fn default() -> Self {
        Self::from_map(HashMap::new())
    }
}

impl TenantTokenEpochs {
    pub fn from_map(map: HashMap<TenantId, u64>) -> Self {
        let (tx, _) = tokio::sync::watch::channel(0u64);
        Self {
            epochs: std::sync::Arc::new(parking_lot::RwLock::new(map)),
            revocation_tx: std::sync::Arc::new(tx),
        }
    }

    pub fn get(&self, tenant_id: &TenantId) -> u64 {
        self.epochs.read().get(tenant_id).copied().unwrap_or(0)
    }

    pub fn set(&self, tenant_id: TenantId, epoch: u64) {
        self.epochs.write().insert(tenant_id, epoch);
        self.revocation_tx.send_modify(|gen| {
            *gen = gen.wrapping_add(1);
        });
    }

    pub fn subscribe_revocations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revocation_tx.subscribe()
    }
}

pub struct BiscuitAuth {
    pubkey: Option<PublicKey>,
    revocation_list: HashSet<Vec<u8>>,
    rules: HashMap<&'static str, AuthRule>,
    lsp_enabled: bool,
    tenant_token_epochs: Option<TenantTokenEpochs>,
}

impl BiscuitAuth {
    pub fn from_pubkey(pubkey: String) -> Result<Self> {
        let pubkey = PublicKey::from_str(&pubkey).context("invalid biscuit public key")?;
        let revocation_list = Default::default();
        let rules = build_rules();
        Ok(Self {
            pubkey: Some(pubkey),
            rules,
            revocation_list,
            lsp_enabled: cfg!(feature = "lsp"),
            tenant_token_epochs: None,
        })
    }

    pub fn without_pubkey() -> Self {
        let revocation_list = Default::default();
        let rules = build_rules();
        Self {
            pubkey: None,
            rules,
            revocation_list,
            lsp_enabled: cfg!(feature = "lsp"),
            tenant_token_epochs: None,
        }
    }

    pub fn with_lsp_enabled(mut self, enabled: bool) -> Self {
        self.lsp_enabled = enabled;
        self
    }

    pub fn with_tenant_token_epochs(mut self, epochs: TenantTokenEpochs) -> Self {
        self.tenant_token_epochs = Some(epochs);
        self
    }

    pub(crate) fn tenant_token_epochs(&self) -> Option<&TenantTokenEpochs> {
        self.tenant_token_epochs.as_ref()
    }

    /// Validate a Bearer token before upgrading an HTTP request to WebSocket:
    /// checks signature, revocation list, Datalog checks (including expiry time),
    /// runtime `lsp_enabled`, and `token_epoch`.
    pub(crate) fn validate_token_for_ws_upgrade(&self, token: &str) -> Result<()> {
        let b = self.extract_biscuit(token)?;
        if b.revocation_identifiers()
            .iter()
            .any(|rev_id| self.revocation_list.contains(rev_id))
        {
            return Err(anyhow::anyhow!("Token is in revocation list"));
        }
        AuthorizerBuilder::new()
            .code("allow if true;")?
            .fact(Fact::new(
                "time".to_string(),
                &[Term::Date(crate::now_timestamp_as_millis_u64() / 1000)],
            ))?
            .build(&b)?
            .authorize()
            .context("token authorization check failed")?;
        let meta = extract_token_metadata(&b)?;
        if let Some(tenant_id) = meta.tenant_id {
            if !cfg!(feature = "lsp") || !self.lsp_enabled {
                return Err(anyhow::anyhow!(
                    "tenant token rejected because hosted LSP service is not enabled"
                ));
            }
            if let Some(epochs) = &self.tenant_token_epochs {
                let current_epoch = epochs.get(&tenant_id);
                let token_epoch = meta.token_epoch.map(|e| e.0).unwrap_or(0);
                if token_epoch < current_epoch {
                    return Err(anyhow::anyhow!(
                        "tenant token has been revoked (epoch {token_epoch} < {current_epoch})"
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn get_rule(&self, method: &str) -> Result<&AuthRule> {
        let Some(rule) = self.rules.get(method) else {
            return Err(anyhow::anyhow!("no rules for method: {method}"));
        };
        Ok(rule)
    }

    pub fn extend_revocation_list(&mut self, list: &[String]) -> Result<()> {
        for rev_id in list {
            let bin_rev_id =
                hex::decode(rev_id.trim_start_matches("0x")).context("Invalid revocation ID")?;
            self.revocation_list.insert(bin_rev_id);
        }
        Ok(())
    }

    pub fn extract_biscuit(&self, token: &str) -> Result<Biscuit> {
        let pubkey = self
            .pubkey
            .as_ref()
            .ok_or_else(|| anyhow!("Biscuit pubkey is empty"))?;
        let b = Biscuit::from_base64(token, pubkey).context("invalid token")?;
        Ok(b)
    }

    /// check permission with time
    ///
    /// - method RPC method
    /// - token biscuit token
    /// - time_in_ms time in milliseconds since UNIX_EPOCH
    pub fn check_permission_with_time(
        &self,
        method: &str,
        token: &str,
        time_in_ms: u64,
    ) -> Result<(Biscuit, &AuthRule)> {
        let b = self.extract_biscuit(token)?;
        // check revocation
        if b.revocation_identifiers()
            .iter()
            .any(|rev_id| self.revocation_list.contains(rev_id))
        {
            tracing::debug!("revoked token: {token}");
            return Err(anyhow::anyhow!("Token is in revocation list: {token}"));
        }
        // Reject tenant tokens when hosted LSP is not enabled at runtime, or when
        // the tenant's token epoch has advanced past this token's epoch.
        let meta = extract_token_metadata(&b)?;
        if let Some(tenant_id) = meta.tenant_id {
            if !cfg!(feature = "lsp") || !self.lsp_enabled {
                return Err(anyhow::anyhow!(
                    "tenant token cannot call {method} because hosted LSP service is not enabled"
                ));
            }
            if let Some(epochs) = &self.tenant_token_epochs {
                let current_epoch = epochs.get(&tenant_id);
                let token_epoch = meta.token_epoch.map(|e| e.0).unwrap_or(0);
                if token_epoch < current_epoch {
                    return Err(anyhow::anyhow!(
                        "tenant token has been revoked (epoch {token_epoch} < {current_epoch})"
                    ));
                }
            }
        }
        // check permission
        let rule = self.get_rule(method)?;
        rule.authorize(&b, time_in_ms)?;
        Ok((b, rule))
    }

    /// check permission
    ///
    /// - method RPC method
    /// - token biscuit token
    pub fn check_permission(&self, method: &str, token: &str) -> Result<(Biscuit, &AuthRule)> {
        self.check_permission_with_time(method, token, now_timestamp_as_millis_u64())
    }
}

/// Authority-block metadata extracted from a Biscuit token in a single `Authorizer` pass.
pub(crate) struct TokenMetadata {
    pub(crate) tenant_id: Option<TenantId>,
    pub(crate) token_epoch: Option<crate::rpc_scope::TokenEpoch>,
    pub(crate) node_id: Option<NodeId>,
}

/// Extract `tenant`, `token_epoch`, and `node` authority facts using one `Authorizer`.
pub(crate) fn extract_token_metadata(token: &Biscuit) -> Result<TokenMetadata> {
    let mut authorizer = build_authorizer(token)?;

    let tenants: Vec<(String,)> = authorizer.query("data($id) <- tenant($id)")?;
    let tenant_id = match tenants.as_slice() {
        [] => None,
        [(tenant_id,)] => Some(TenantId::new(tenant_id.clone()).map_err(anyhow::Error::msg)?),
        _ => {
            return Err(anyhow!(
                "token authority block must contain at most one tenant fact"
            ))
        }
    };

    let epochs: Vec<(i64,)> = authorizer.query("data($epoch) <- token_epoch($epoch)")?;
    let token_epoch = match epochs.as_slice() {
        [] => None,
        [(epoch,)] => Some(crate::rpc_scope::TokenEpoch(
            u64::try_from(*epoch)
                .map_err(|_| anyhow!("token authority block contains negative token_epoch"))?,
        )),
        _ => {
            return Err(anyhow!(
                "token authority block must contain at most one token_epoch fact"
            ))
        }
    };

    let nodes: Vec<(String,)> = authorizer.query("data($id) <- node($id)")?;
    let node_id = match nodes.as_slice() {
        [] => None,
        [(id,)] => Some(NodeId::from_str(id.as_str())?),
        _ => {
            return Err(anyhow!(
                "token authority block must contain at most one node fact"
            ))
        }
    };

    Ok(TokenMetadata {
        tenant_id,
        token_epoch,
        node_id,
    })
}

/// Extract node id from token
pub fn extract_node_id(token: &Biscuit) -> Result<NodeId> {
    extract_optional_node_id(token)?.ok_or_else(|| anyhow!("token is missing a node fact"))
}

pub(crate) fn extract_optional_node_id(token: &Biscuit) -> Result<Option<NodeId>> {
    const QUERY: &str = "data($id) <- node($id)";
    let mut authorizer = build_authorizer(token)?;
    let nodes: Vec<(String,)> = authorizer.query(QUERY)?;
    match nodes.as_slice() {
        [] => Ok(None),
        [(id,)] => Ok(Some(NodeId::from_str(id.as_str())?)),
        _ => Err(anyhow!(
            "token authority block must contain at most one node fact"
        )),
    }
}

/// Extract the hosted tenant identity from the token authority block.
///
/// Biscuit holders may append attenuation blocks, so tenant facts from those
/// blocks must not be trusted for request routing. `Authorizer::query` only
/// exposes authority facts unless an additional trust scope is requested.
pub fn extract_tenant_id(token: &Biscuit) -> Result<Option<TenantId>> {
    const QUERY: &str = "data($id) <- tenant($id)";
    let mut authorizer = build_authorizer(token)?;
    let tenants: Vec<(String,)> = authorizer.query(QUERY)?;
    match tenants.as_slice() {
        [] => Ok(None),
        [(tenant_id,)] => TenantId::new(tenant_id.clone())
            .map(Some)
            .map_err(anyhow::Error::msg),
        _ => Err(anyhow!(
            "token authority block must contain at most one tenant fact"
        )),
    }
}

/// Extract the revocation `token_epoch` fact from the token authority block.
pub fn extract_token_epoch(token: &Biscuit) -> Result<Option<u64>> {
    const QUERY: &str = "data($epoch) <- token_epoch($epoch)";
    let mut authorizer = build_authorizer(token)?;
    let epochs: Vec<(i64,)> = authorizer.query(QUERY)?;
    match epochs.as_slice() {
        [] => Ok(None),
        [(epoch,)] => u64::try_from(*epoch)
            .map(Some)
            .map_err(|_| anyhow!("token authority block contains negative token_epoch")),
        _ => Err(anyhow!(
            "token authority block must contain at most one token_epoch fact"
        )),
    }
}

#[cfg(test)]
mod tests;
