use anyhow::{anyhow, Context, Result};
use biscuit_auth::{
    builder::{Fact, Term},
    AuthorizerBuilder, Biscuit, PublicKey,
};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use crate::now_timestamp_as_millis_u64;
use fiber_types::NodeId;

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
        let authorizer = AuthorizerBuilder::new()
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
    b.rule("receive_btc", r#"allow if read("cch");"#);
    b.rule("get_cch_order", r#"allow if read("cch");"#);
    // channels
    b.rule("open_channel", r#"allow if write("channels");"#);
    b.rule("accept_channel", r#"allow if write("channels");"#);
    b.rule("abandon_channel", r#"allow if write("channels");"#);
    b.rule("list_channels", r#"allow if read("channels");"#);
    b.rule("shutdown_channel", r#"allow if write("channels");"#);
    b.rule("update_channel", r#"allow if write("channels");"#);
    // dev
    b.rule("commitment_signed", r#"allow if write("messages");"#);
    b.rule("add_tlc", r#"allow if write("channels");"#);
    b.rule("remove_tlc", r#"allow if write("channels");"#);
    b.rule("check_channel_shutdown", r#"allow if write("channels");"#);
    b.rule(
        "submit_commitment_transaction",
        r#"allow if write("chain");"#,
    );
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

    b.0
}

pub struct BiscuitAuth {
    pubkey: Option<PublicKey>,
    revocation_list: HashSet<Vec<u8>>,
    rules: HashMap<&'static str, AuthRule>,
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
        })
    }

    pub fn without_pubkey() -> Self {
        let revocation_list = Default::default();
        let rules = build_rules();
        Self {
            pubkey: None,
            rules,
            revocation_list,
        }
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
        // check permission
        let rule = self.get_rule(method)?;
        if let Err(err) = rule.authorize(&b, time_in_ms) {
            tracing::debug!("authorize failed: {err}");
            return Err(err);
        }
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

/// Extract node id from token
pub fn extract_node_id(token: &Biscuit) -> Result<NodeId> {
    const QUERY: &str = "data($id) <- node($id)";
    let (id,): (String,) = token.authorizer()?.query_exactly_one(QUERY)?;
    let node_id = NodeId::from_str(id.as_str())?;
    tracing::warn!("fetch {id:?} {node_id:?}");
    Ok(node_id)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use biscuit_auth::{macros::biscuit, KeyPair};

    use crate::rpc::biscuit::extract_node_id;

    use super::BiscuitAuth;

    #[test]
    fn test_biscuit_auth() {
        let root = KeyPair::new();

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit
        let token = biscuit!(
            r#"
          write("payments");
          read("peers");
    "#
        )
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

        // check permission
        assert!(auth.check_permission("send_payment", &token,).is_ok());
        // write permission do not implies read
        assert!(auth.check_permission("get_payment", &token).is_err());
        assert!(auth.check_permission("list_peers", &token).is_ok());
        assert!(auth.check_permission("connect_peer", &token).is_err());

        // if not match any rule, it should be denied
        assert!(auth.check_permission("unknown", &token).is_err());
    }

    #[test]
    fn test_biscuit_auth_with_wrong_token() {
        let root = KeyPair::new();
        let invalid_root = KeyPair::new();
        assert_ne!(root.public(), invalid_root.public());

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit
        let token = biscuit!(
            r#"
          write("payments");
    "#
        )
        .build(&invalid_root)
        .unwrap()
        .to_base64()
        .unwrap();

        // check permission
        assert!(auth.check_permission("send_payment", &token).is_err());
    }

    #[test]
    fn test_biscuit_auth_watchtower() {
        let root = KeyPair::new();

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit
        let token1 = biscuit!(
            r#"
          write("watchtower");
    "#
        )
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

        // check permission
        assert!(auth.check_permission("update_revocation", &token1,).is_ok());
    }

    #[test]
    fn test_biscuit_token_timeout() {
        let root = KeyPair::new();

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit with timeout
        let token = biscuit!(
            r#"
                write("payments");
                check if time($time), $time <= 2022-03-30T20:00:00Z;
    "#
        )
        .build(&root)
        .unwrap()
        .to_base64()
        .unwrap();

        // check permission
        let future_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let past_time = Duration::from_millis(10).as_millis() as u64;
        assert!(auth
            .check_permission_with_time("send_payment", &token, future_time)
            .is_err());
        assert!(auth
            .check_permission_with_time("send_payment", &token, past_time)
            .is_ok());
    }

    #[test]
    fn test_biscuit_revocation() {
        let root = KeyPair::new();

        // auth
        let mut auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit
        let token = {
            let biscuit = biscuit!(
                r#"
          write("payments");
          read("peers");
    "#
            )
            .build(&root)
            .unwrap();

            biscuit.to_base64().unwrap()
        };

        // sign a exactly same content revoked token
        let (rev_token, rev_id) = {
            let biscuit = biscuit!(
                r#"
          write("payments");
          read("peers");
    "#
            )
            .build(&root)
            .unwrap();
            let rev_id = hex::encode(&biscuit.revocation_identifiers()[0]);

            (biscuit.to_base64().unwrap(), rev_id)
        };
        auth.extend_revocation_list(&[rev_id]).unwrap();

        // check permission
        assert!(auth.check_permission("send_payment", &token,).is_ok());
        // write permission do not implies read
        assert!(auth.check_permission("list_peers", &token).is_ok());

        // check revoked token
        assert!(auth.check_permission("send_payment", &rev_token,).is_err());
        // write permission do not implies read
        assert!(auth.check_permission("list_peers", &rev_token).is_err());
        assert!(auth.check_permission("get_payment", &token).is_err());

        // if not match any rule, it should be denied
        assert!(auth.check_permission("unknown", &token).is_err());
        assert!(auth.check_permission("unknown", &rev_token).is_err());
    }

    #[test]
    fn test_parse_biscuit_token() {
        // Pubkey
        let pubkey = "ed25519/17b172749be74276f0ed35a5d0685752684a3c5722114bba447a2f301136db79";
        // token with all permission
        let token = "EpoDCq8CCi5RbWJ2UmpKSEFRRG1qM2NnblVCR1E1elZuR3hVS3diMnFKeWd3TnMyd2s0MWg4CgNjY2gKCGNoYW5uZWxzCghtZXNzYWdlcwoFY2hhaW4KBWdyYXBoCghpbnZvaWNlcwoIcGF5bWVudHMKBXBlZXJzCgp3YXRjaHRvd2VyGAMiCQoHCBgSAxiACCIJCgcIARIDGIEIIgkKBwgAEgMYgQgiCQoHCAESAxiCCCIJCgcIABIDGIIIIgkKBwgBEgMYgwgiCQoHCAESAxiECCIJCgcIABIDGIUIIggKBggAEgIYGCIJCgcIARIDGIYIIgkKBwgAEgMYhggiCQoHCAESAxiHCCIJCgcIABIDGIcIIgkKBwgBEgMYiAgiCQoHCAASAxiICCIJCgcIARIDGIkIEiQIABIgdmX3T0R0Fu6vXZUeVD21AZU7K85xSAmsFAOQSqBd1aYaQNdmA5kqLQ6qNVmOxB0uiEFjUq_OQ0DkZgtYt0mSHktbpubKHzJo-imaFx6Pz7f4NzkOvcLLLFUwBlyHxGyEkQAiIgogAEX-eNhS6VprsYL7moBGaJeVUAnn5VzKLr77Rz9y3AI=";
        // auth
        let auth = BiscuitAuth::from_pubkey(pubkey.to_string()).unwrap();

        // check permission
        assert!(auth.check_permission("send_payment", token).is_ok());
        // write permission do not implies read
        assert!(auth.check_permission("get_payment", token).is_ok());
        assert!(auth.check_permission("list_peers", token).is_ok());
        assert!(auth.check_permission("connect_peer", token).is_ok());

        // if not match any rule, it should be denied
        assert!(auth.check_permission("unknown", token).is_err());

        let b = auth.extract_biscuit(token).unwrap();
        let node_id = extract_node_id(&b).unwrap();
        assert!(!node_id.as_ref().is_empty());
        println!("node_id: {node_id:?}");
    }
}
