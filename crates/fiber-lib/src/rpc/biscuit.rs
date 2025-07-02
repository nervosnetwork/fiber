use anyhow::{Context, Result};
use biscuit_auth::{
    builder::{Fact, Term},
    AuthorizerBuilder, Biscuit, PublicKey,
};
use std::{collections::HashMap, str::FromStr};

use crate::now_timestamp_as_millis_u64;

struct AuthRule {
    code: &'static str,
}

impl AuthRule {
    fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// build rule
    ///
    /// - req_params RPC method parameters
    fn build_rule(&self, req_params: serde_json::Value) -> Result<AuthorizerBuilder> {
        let mut params = HashMap::new();
        for (param, value) in req_params.as_object().context("invalid parameter")? {
            // only support string for now
            // the value maybe contains unexpected injection,
            // but it will fail when parse parameter to concrete type in RPC methods
            if let serde_json::Value::String(s) = value {
                params.insert(param.to_string(), Term::Str(s.to_owned()));
            }
        }
        let scope_params = HashMap::new();
        let authorizer =
            AuthorizerBuilder::new().code_with_params(self.code, params, scope_params)?;
        Ok(authorizer)
    }

    /// authorize
    ///
    /// - params RPC method parameters
    /// - token biscuit token
    /// - time_in_ms time in milliseconds since UNIX_EPOCH
    fn authorize(&self, params: serde_json::Value, token: Biscuit, time_in_ms: u64) -> Result<()> {
        self.build_rule(params)?
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
        allow if right({channel_id}, "watchtower");
        "#,
    );
    rule(
        "remove_watch_channel",
        r#"
        allow if write("watchtower");
        allow if right({channel_id}, "watchtower");
        "#,
    );
    rule(
        "update_revocation",
        r#"
        allow if write("watchtower");
        allow if right({channel_id}, "watchtower");
        "#,
    );
    rule(
        "update_local_settlement",
        r#"
        allow if write("watchtower");
        allow if right({channel_id}, "watchtower");
        "#,
    );
    rule("create_preimage", r#"allow if write("watchtower");"#);
    rule("remove_preimage", r#"allow if write("watchtower");"#);

    rules
}

pub(crate) struct BiscuitAuth {
    pubkey: PublicKey,
    rules: HashMap<&'static str, AuthRule>,
}

impl BiscuitAuth {
    pub fn from_pubkey(pubkey: String) -> Result<Self> {
        let pubkey = PublicKey::from_str(&pubkey).context("invalid biscuit public key")?;
        let rules = build_rules();
        Ok(Self { pubkey, rules })
    }

    /// check permission with time
    ///
    /// - method RPC method
    /// - params RPC method parameters
    /// - token biscuit token
    /// - time_in_ms time in milliseconds since UNIX_EPOCH
    pub fn check_permission_with_time(
        &self,
        method: &str,
        params: serde_json::Value,
        token: &[u8],
        time_in_ms: u64,
    ) -> Result<()> {
        let b = Biscuit::from(token, self.pubkey).context("invalid token")?;
        let Some(rule) = self.rules.get(method) else {
            return Err(anyhow::anyhow!("no rules for method: {method}"));
        };
        rule.authorize(params, b, time_in_ms)
    }

    /// check permission
    ///
    /// - method RPC method
    /// - params RPC method parameters
    /// - token biscuit token
    pub fn check_permission(
        &self,
        method: &str,
        params: serde_json::Value,
        token: &[u8],
    ) -> Result<()> {
        self.check_permission_with_time(method, params, token, now_timestamp_as_millis_u64())
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

            biscuit.to_vec().unwrap()
        };

        // check permission
        assert!(auth
            .check_permission("send_payment", json!({}), &token,)
            .is_ok());
        // write permission do not implies read
        assert!(auth
            .check_permission("get_payment", json!({}), &token)
            .is_err());
        assert!(auth
            .check_permission("list_peers", json!({}), &token)
            .is_ok());
        assert!(auth
            .check_permission("connect_peer", json!({}), &token)
            .is_err());

        // if not match any rule, it should be denied
        assert!(auth.check_permission("unknown", json!({}), &token).is_err());
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

            biscuit.to_vec().unwrap()
        };

        // check permission
        assert!(auth
            .check_permission("send_payment", json!({}), &token)
            .is_err());
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

            biscuit.to_vec().unwrap()
        };

        let token2 = {
            let biscuit = biscuit!(
                r#"
          right("a_channel_id", "watchtower");
    "#
            )
            .build(&root)
            .unwrap();

            biscuit.to_vec().unwrap()
        };

        let token3 = {
            let biscuit = biscuit!(
                r#"
          right("b_channel_id", "watchtower");
    "#
            )
            .build(&root)
            .unwrap();
            biscuit.to_vec().unwrap()
        };

        // check permission

        // token1 with write(watchtower) can operate all channels
        auth.check_permission(
            "update_revocation",
            json!({"channel_id": "a_channel_id"}),
            &token1,
        )
        .unwrap();
        assert!(auth
            .check_permission(
                "update_revocation",
                json!({"channel_id": "a_channel_id"}),
                &token1,
            )
            .is_ok());
        assert!(auth
            .check_permission(
                "update_revocation",
                json!({"channel_id": "b_channel_id"}),
                &token1,
            )
            .is_ok());

        // token2 with write(watchtower/a_channel_id) can only operate a_channel_id
        assert!(auth
            .check_permission(
                "update_revocation",
                json!({"channel_id": "a_channel_id"}),
                &token2,
            )
            .is_ok());
        assert!(auth
            .check_permission(
                "update_revocation",
                json!({"channel_id": "b_channel_id"}),
                &token2,
            )
            .is_err());

        // token3 with write(watchtower/b_channel_id) can only operate b_channel_id
        assert!(auth
            .check_permission(
                "update_revocation",
                json!({"channel_id": "b_channel_id"}),
                &token3,
            )
            .is_ok());
        assert!(auth
            .check_permission(
                "update_revocation",
                json!({"channel_id": "a_channel_id"}),
                &token3,
            )
            .is_err());
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

            biscuit.to_vec().unwrap()
        };

        // check permission
        let future_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let past_time = Duration::from_millis(10).as_millis() as u64;
        assert!(auth
            .check_permission_with_time("send_payment", json!({}), &token, future_time)
            .is_err());
        assert!(auth
            .check_permission_with_time("send_payment", json!({}), &token, past_time)
            .is_ok());
    }
}
