use anyhow::{Context, Result};
use biscuit_auth::{
    builder::{Fact, Term},
    AuthorizerBuilder, Biscuit, PublicKey,
};
use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use crate::now_timestamp_as_millis_u64;

struct AuthRule {
    code: &'static str,
}

impl AuthRule {
    fn new(code: &'static str) -> Self {
        Self { code }
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
    fn authorize(&self, token: Biscuit, time_in_ms: u64) -> Result<()> {
        self.build_rule()?
            .fact(Fact::new(
                "time".to_string(),
                &[Term::Date(time_in_ms / 1000)],
            ))?
            .build(&token)?
            .authorize()?;
        Ok(())
    }
}

fn build_rules() -> HashMap<&'static str, AuthRule> {
    let mut rules = HashMap::new();
    let rules_mut_ref = &mut rules;

    let mut rule = move |method: &'static str, code: &'static str| {
        rules_mut_ref.insert(method, AuthRule::new(code));
    };

    // Cch
    rule("send_btc", r#"allow if write("cch");"#);
    rule("receive_btc", r#"allow if read("cch");"#);
    rule("get_receive_btc_order", r#"allow if read("cch");"#);
    // channels
    rule("open_channel", r#"allow if write("channels");"#);
    rule("accept_channel", r#"allow if write("channels");"#);
    rule("abandon_channel", r#"allow if write("channels");"#);
    rule("list_channels", r#"allow if read("channels");"#);
    rule("shutdown_channel", r#"allow if write("channels");"#);
    rule("update_channel", r#"allow if write("channels");"#);
    // dev
    rule("commitment_signed", r#"allow if write("messages");"#);
    rule("add_tlc", r#"allow if write("channels");"#);
    rule("remove_tlc", r#"allow if write("channels");"#);
    rule(
        "submit_commitment_transaction",
        r#"allow if write("chain");"#,
    );
    // graph
    rule("graph_nodes", r#"allow if read("graph");"#);
    rule("graph_channels", r#"allow if read("graph");"#);

    // info
    rule("node_info", r#"allow if read("node");"#);
    rule("new_invoice", r#"allow if write("invoices");"#);
    rule("parse_invoice", r#"allow if read("invoices");"#);
    rule("get_invoice", r#"allow if read("invoices");"#);
    rule("cancel_invoice", r#"allow if write("invoices");"#);

    // payment
    rule("send_payment", r#"allow if write("payments");"#);
    rule("get_payment", r#"allow if read("payments");"#);
    rule("build_router", r#"allow if read("payments");"#);
    rule("send_payment_with_router", r#"allow if write("payments");"#);

    // peer
    rule("connect_peer", r#"allow if write("peers");"#);
    rule("disconnect_peer", r#"allow if write("peers");"#);
    rule("list_peers", r#"allow if read("peers");"#);

    // watchtower
    rule(
        "create_watch_channel",
        r#"
        allow if write("watchtower");
        "#,
    );
    rule(
        "remove_watch_channel",
        r#"
        allow if write("watchtower");
        "#,
    );
    rule(
        "update_revocation",
        r#"
        allow if write("watchtower");
        "#,
    );
    rule(
        "update_local_settlement",
        r#"
        allow if write("watchtower");
        "#,
    );
    rule("create_preimage", r#"allow if write("watchtower");"#);
    rule("remove_preimage", r#"allow if write("watchtower");"#);

    rules
}

pub struct BiscuitAuth {
    pubkey: PublicKey,
    revocation_list: HashSet<Vec<u8>>,
    rules: HashMap<&'static str, AuthRule>,
}

impl BiscuitAuth {
    pub fn from_pubkey(pubkey: String) -> Result<Self> {
        let pubkey = PublicKey::from_str(&pubkey).context("invalid biscuit public key")?;
        let revocation_list = Default::default();
        let rules = build_rules();
        Ok(Self {
            pubkey,
            rules,
            revocation_list,
        })
    }

    pub fn extend_revocation_list(&mut self, list: &[String]) -> Result<()> {
        for rev_id in list {
            let bin_rev_id =
                hex::decode(rev_id.trim_start_matches("0x")).context("Invalid revocation ID")?;
            self.revocation_list.insert(bin_rev_id);
        }
        Ok(())
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
    ) -> Result<()> {
        let b = Biscuit::from_base64(token, self.pubkey).context("invalid token")?;
        // check revocation
        if b.revocation_identifiers()
            .iter()
            .any(|rev_id| self.revocation_list.contains(rev_id))
        {
            tracing::debug!("revoked token: {token}");
            return Err(anyhow::anyhow!("Token is in revocation list: {token}"));
        }
        // check permission
        let Some(rule) = self.rules.get(method) else {
            return Err(anyhow::anyhow!("no rules for method: {method}"));
        };
        if let Err(err) = rule.authorize(b, time_in_ms) {
            tracing::debug!("authorize failed: {err}");
            return Err(err);
        }
        Ok(())
    }

    /// check permission
    ///
    /// - method RPC method
    /// - token biscuit token
    pub fn check_permission(&self, method: &str, token: &str) -> Result<()> {
        self.check_permission_with_time(method, token, now_timestamp_as_millis_u64())
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use biscuit_auth::{macros::biscuit, KeyPair};
    use serde_json::json;

    use super::BiscuitAuth;

    #[test]
    fn test_biscuit_auth() {
        let root = KeyPair::new();

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

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

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit
        let token = {
            let biscuit = biscuit!(
                r#"
          write("payments");
    "#
            )
            .build(&invalid_root)
            .unwrap();

            biscuit.to_base64().unwrap()
        };

        // check permission
        assert!(auth.check_permission("send_payment", &token).is_err());
    }

    #[test]
    fn test_biscuit_auth_channel() {
        let root = KeyPair::new();

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit
        let token1 = {
            let biscuit = biscuit!(
                r#"
          write("watchtower");
    "#
            )
            .build(&root)
            .unwrap();

            biscuit.to_base64().unwrap()
        };

        // check permission

        assert!(auth
            .check_permission(
                "update_revocation",
                &token1,
            )
            .is_ok());
    }

    #[test]
    fn test_biscuit_token_timeout() {
        let root = KeyPair::new();

        // auth
        let auth = BiscuitAuth::from_pubkey(root.public().to_string()).unwrap();

        // sign a biscuit with timeout
        let token = {
            let biscuit = biscuit!(
                r#"
                write("payments");
                check if time($time), $time <= 2022-03-30T20:00:00Z;
    "#
            )
            .build(&root)
            .unwrap();

            biscuit.to_base64().unwrap()
        };

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

}
