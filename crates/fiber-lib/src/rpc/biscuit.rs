use anyhow::{Context, Result};
use biscuit_auth::{macros::authorizer, AuthorizerBuilder, Biscuit, PublicKey};
use std::{collections::HashMap, str::FromStr};

fn build_rules() -> HashMap<&'static str, AuthorizerBuilder> {
    let mut rules = HashMap::new();

    // Cch
    rules.insert("send_btc", authorizer!(r#"allow if write("cch");"#));
    rules.insert("receive_btc", authorizer!(r#"allow if read("cch");"#));
    rules.insert(
        "get_receive_btc_order",
        authorizer!(r#"allow if read("cch");"#),
    );
    // channels
    rules.insert("open_channel", authorizer!(r#"allow if write("channel");"#));
    rules.insert(
        "accept_channel",
        authorizer!(r#"allow if write("channel");"#),
    );
    rules.insert(
        "abandon_channel",
        authorizer!(r#"allow if write("channel");"#),
    );
    rules.insert("list_channels", authorizer!(r#"allow if read("channel");"#));
    rules.insert(
        "shutdown_channel",
        authorizer!(r#"allow if write("channel");"#),
    );
    rules.insert(
        "update_channel",
        authorizer!(r#"allow if write("channel");"#),
    );
    // dev
    rules.insert(
        "commitment_signed",
        authorizer!(r#"allow if write("message");"#),
    );
    rules.insert("add_tlc", authorizer!(r#"allow if write("channel");"#));
    rules.insert("remove_tlc", authorizer!(r#"allow if write("channel");"#));
    rules.insert(
        "submit_commitment_transaction",
        authorizer!(r#"allow if write("chain");"#),
    );
    // graph
    rules.insert("graph_nodes", authorizer!(r#"allow if read("graph");"#));
    rules.insert("graph_channels", authorizer!(r#"allow if read("graph");"#));

    // info
    rules.insert("node_info", authorizer!(r#"allow if read("node");"#));

    // invoice
    rules.insert("new_invoice", authorizer!(r#"allow if write("invoice");"#));
    rules.insert("parse_invoice", authorizer!(r#"allow if read("invoice");"#));
    rules.insert("get_invoice", authorizer!(r#"allow if read("invoice");"#));
    rules.insert(
        "cancel_invoice",
        authorizer!(r#"allow if read("invoice");"#),
    );

    // payment
    rules.insert("send_payment", authorizer!(r#"allow if write("payment");"#));
    rules.insert("get_payment", authorizer!(r#"allow if read("payment");"#));
    rules.insert("build_router", authorizer!(r#"allow if read("payment");"#));
    rules.insert(
        "send_payment_with_router",
        authorizer!(r#"allow if write("payment");"#),
    );

    // peer
    rules.insert("connect_peer", authorizer!(r#"allow if write("peer");"#));
    rules.insert("disconnect_peer", authorizer!(r#"allow if write("peer");"#));
    rules.insert("list_peers", authorizer!(r#"allow if read("peer");"#));

    // watchtower
    rules.insert(
        "create_watch_channel",
        authorizer!(r#"allow if write("watchtower");"#),
    );
    rules.insert(
        "remove_watch_channel",
        authorizer!(r#"allow if write("watchtower");"#),
    );
    rules.insert(
        "update_revocation",
        authorizer!(r#"allow if write("watchtower");"#),
    );
    rules.insert(
        "update_local_settlement",
        authorizer!(r#"allow if write("watchtower");"#),
    );
    rules.insert(
        "create_preimage",
        authorizer!(r#"allow if write("watchtower");"#),
    );
    rules.insert(
        "remove_preimage",
        authorizer!(r#"allow if write("watchtower");"#),
    );

    rules
}

pub(crate) struct BiscuitAuth {
    pubkey: PublicKey,
    rules: HashMap<&'static str, AuthorizerBuilder>,
}

impl BiscuitAuth {
    pub fn from_pubkey(pubkey: String) -> Result<Self> {
        let pubkey = PublicKey::from_str(&pubkey).context("invalid biscuit public key")?;
        let rules = build_rules();
        Ok(Self { pubkey, rules })
    }

    pub fn check_permission(&self, method: &str, token: &[u8]) -> Result<()> {
        let b = Biscuit::from(token, self.pubkey).context("invalid biscuit")?;
        let Some(rules) = self.rules.get(method) else {
            return Err(anyhow::anyhow!("no rules for method: {method}"));
        };
        rules.clone().build(&b)?.authorize()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use biscuit_auth::{macros::biscuit, KeyPair};

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
          write("payment");
          read("peer");
    "#
            )
            .build(&root)
            .unwrap();

            biscuit.to_vec().unwrap()
        };

        // check permission
        assert!(auth.check_permission("send_payment", &token).is_ok());
        // write permission do not implies read
        assert!(auth.check_permission("get_payment", &token).is_err());
        assert!(auth.check_permission("list_peers", &token).is_ok());
        assert!(auth.check_permission("connect_peer", &token).is_err());

        // if not match any rule, it should be denied
        assert!(auth.check_permission("unknown", &token).is_err());
    }
}
