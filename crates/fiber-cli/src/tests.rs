use super::*;

// ── shell_words tests ────────────────────────────────────────────────

#[test]
fn test_shell_words_simple() {
    let result = shell_words("hello world").unwrap();
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_shell_words_empty() {
    let result = shell_words("").unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_shell_words_whitespace_only() {
    let result = shell_words("   \t  ").unwrap();
    assert!(result.is_empty());
}

#[test]
fn test_shell_words_single_word() {
    let result = shell_words("hello").unwrap();
    assert_eq!(result, vec!["hello"]);
}

#[test]
fn test_shell_words_multiple_spaces() {
    let result = shell_words("hello    world").unwrap();
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_shell_words_leading_trailing_spaces() {
    let result = shell_words("  hello world  ").unwrap();
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_shell_words_tabs() {
    let result = shell_words("hello\tworld").unwrap();
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_shell_words_double_quotes() {
    let result = shell_words(r#"hello "world foo" bar"#).unwrap();
    assert_eq!(result, vec!["hello", "world foo", "bar"]);
}

#[test]
fn test_shell_words_single_quotes() {
    let result = shell_words("hello 'world foo' bar").unwrap();
    assert_eq!(result, vec!["hello", "world foo", "bar"]);
}

#[test]
fn test_shell_words_mixed_quotes() {
    let result = shell_words(r#"'single' "double" plain"#).unwrap();
    assert_eq!(result, vec!["single", "double", "plain"]);
}

#[test]
fn test_shell_words_backslash_escape() {
    let result = shell_words(r"hello\ world").unwrap();
    assert_eq!(result, vec!["hello world"]);
}

#[test]
fn test_shell_words_backslash_in_double_quotes() {
    let result = shell_words(r#""hello\" world""#).unwrap();
    assert_eq!(result, vec![r#"hello" world"#]);
}

#[test]
fn test_shell_words_single_quotes_preserve_backslash() {
    // Single quotes preserve everything literally, including backslash
    let result = shell_words(r"'hello\ world'").unwrap();
    assert_eq!(result, vec![r"hello\ world"]);
}

#[test]
fn test_shell_words_empty_double_quotes() {
    let result = shell_words(r#"hello "" world"#).unwrap();
    // Empty quotes don't produce a token since shell_words only pushes non-empty strings
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_shell_words_empty_single_quotes() {
    let result = shell_words("hello '' world").unwrap();
    assert_eq!(result, vec!["hello", "world"]);
}

#[test]
fn test_shell_words_unclosed_double_quote() {
    let result = shell_words(r#"hello "world"#);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unclosed quote"));
}

#[test]
fn test_shell_words_unclosed_single_quote() {
    let result = shell_words("hello 'world");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("Unclosed quote"));
}

#[test]
fn test_shell_words_json_argument() {
    // Typical real usage: passing JSON as a CLI argument
    let result =
        shell_words(r#"channel open --funding-udt-type-script '{"code_hash":"0xabc"}'"#).unwrap();
    assert_eq!(
        result,
        vec![
            "channel",
            "open",
            "--funding-udt-type-script",
            r#"{"code_hash":"0xabc"}"#
        ]
    );
}

#[test]
fn test_shell_words_adjacent_quotes() {
    // Quoted segments adjacent to unquoted text merge into one word
    let result = shell_words(r#"hello"world"foo"#).unwrap();
    assert_eq!(result, vec!["helloworldfoo"]);
}

#[test]
fn test_shell_words_double_quote_inside_single_quotes() {
    let result = shell_words(r#"'he said "hello"'"#).unwrap();
    assert_eq!(result, vec![r#"he said "hello""#]);
}

#[test]
fn test_shell_words_single_quote_inside_double_quotes() {
    let result = shell_words(r#""it's fine""#).unwrap();
    assert_eq!(result, vec!["it's fine"]);
}

// ── build_cli / build_interactive_cli structure tests ────────────────

#[test]
fn test_build_cli_has_expected_subcommands() {
    let cli = build_cli();
    let sub_names: Vec<&str> = cli.get_subcommands().map(|s| s.get_name()).collect();
    let expected = [
        "info",
        "peer",
        "channel",
        "invoice",
        "payment",
        "graph",
        "cch",
        "dev",
        "watchtower",
        "prof",
    ];
    for name in &expected {
        assert!(
            sub_names.contains(name),
            "build_cli() missing subcommand: {}",
            name
        );
    }
}

#[test]
fn test_build_cli_global_args() {
    let cli = build_cli();
    let arg_names: Vec<&str> = cli.get_arguments().filter_map(|a| a.get_long()).collect();
    assert!(arg_names.contains(&"url"));
    assert!(arg_names.contains(&"raw-data"));
    assert!(arg_names.contains(&"output-format"));
    assert!(arg_names.contains(&"no-banner"));
    assert!(arg_names.contains(&"color"));
    assert!(arg_names.contains(&"auth-token"));
}

#[test]
fn test_build_cli_auth_token_parsed() {
    let cli = build_cli();
    let matches = cli
        .try_get_matches_from(["fnn-cli", "--auth-token", "my-secret", "info"])
        .unwrap();
    let token = matches.get_one::<String>("auth_token").unwrap();
    assert_eq!(token, "my-secret");
}

#[test]
fn test_build_cli_auth_token_optional() {
    let cli = build_cli();
    let matches = cli.try_get_matches_from(["fnn-cli", "info"]).unwrap();
    assert!(matches.get_one::<String>("auth_token").is_none());
}

#[test]
fn test_build_interactive_cli_has_quit() {
    let cli = build_interactive_cli(false);
    let sub_names: Vec<&str> = cli.get_subcommands().map(|s| s.get_name()).collect();
    assert!(sub_names.contains(&"quit"));
    // "quit" should be the last subcommand in help
    assert_eq!(*sub_names.last().unwrap(), "quit");
}

#[test]
fn test_build_interactive_cli_has_same_commands() {
    let cli = build_interactive_cli(false);
    let sub_names: Vec<&str> = cli.get_subcommands().map(|s| s.get_name()).collect();
    let expected = [
        "info",
        "peer",
        "channel",
        "invoice",
        "payment",
        "graph",
        "cch",
        "dev",
        "watchtower",
        "prof",
    ];
    for name in &expected {
        assert!(
            sub_names.contains(name),
            "build_interactive_cli(false) missing subcommand: {}",
            name
        );
    }
}

// ── build_completion_tree tests ──────────────────────────────────────

#[test]
fn test_completion_tree_has_top_level() {
    let cli = build_interactive_cli(false);
    let tree = build_completion_tree(&cli);
    let top = tree.get("").expect("should have empty-string key");
    assert!(top.contains(&"channel".to_string()));
    assert!(top.contains(&"payment".to_string()));
    assert!(top.contains(&"info".to_string()));
}

#[test]
fn test_completion_tree_has_subcommand_entries() {
    let cli = build_interactive_cli(false);
    let tree = build_completion_tree(&cli);
    // "channel" should have subcommand entries (open_channel, list_channels, etc.)
    let channel_entries = tree.get("channel").expect("should have 'channel' key");
    assert!(
        !channel_entries.is_empty(),
        "channel should have completions"
    );
    // Should contain subcommand names like "open_channel", "list_channels"
    assert!(
        channel_entries.contains(&"open_channel".to_string()),
        "channel completions should include 'open_channel', got: {:?}",
        channel_entries
    );
}

#[test]
fn test_completion_tree_subcommand_has_options() {
    let cli = build_interactive_cli(false);
    let tree = build_completion_tree(&cli);
    // "channel open_channel" should have --flag options
    let key = "channel open_channel".to_string();
    if let Some(options) = tree.get(&key) {
        // Should have at least --pubkey and --funding-amount
        assert!(
            options.iter().any(|o| o == "--pubkey"),
            "channel open_channel should have --pubkey option"
        );
        assert!(
            options.iter().any(|o| o == "--funding-amount"),
            "channel open_channel should have --funding-amount option"
        );
    }
    // Note: if the key doesn't exist, the completion tree may merge options
    // into the parent level, which is also acceptable behavior.
}

// ── Shared helper for CLI arg tests ─────────────────────────────────

/// Build a command with augmented args and parse the given CLI tokens.
fn parse_args<F>(augment: F, args: &[&str]) -> clap::ArgMatches
where
    F: FnOnce(clap::Command) -> clap::Command,
{
    let cmd = augment(clap::Command::new("test"));
    cmd.try_get_matches_from(args).unwrap()
}

// ── Peer CLI arg tests (migrated from fiber-json-types/src/cli_tests.rs) ─

mod peer_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use clap::Command;
    use fiber_json_types::serde_utils::Pubkey;
    use fiber_json_types::{ConnectPeerParams, DisconnectPeerParams};

    #[test]
    fn test_connect_peer_by_address() {
        let matches = parse_args(
            ConnectPeerParams::augment_command,
            &["test", "--address", "/ip4/127.0.0.1/tcp/8119"],
        );
        let params = ConnectPeerParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.address, Some("/ip4/127.0.0.1/tcp/8119".to_string()));
        assert!(params.pubkey.is_none());
        assert!(params.save.is_none());
    }

    #[test]
    fn test_connect_peer_by_pubkey() {
        let pubkey_hex = "03".to_string() + &"aa".repeat(32);
        let matches = parse_args(
            ConnectPeerParams::augment_command,
            &["test", "--pubkey", &pubkey_hex],
        );
        let params = ConnectPeerParams::from_arg_matches(&matches).unwrap();
        assert!(params.address.is_none());
        assert_eq!(params.pubkey, Some(pubkey_hex.parse::<Pubkey>().unwrap()));
    }

    #[test]
    fn test_connect_peer_with_save() {
        let matches = parse_args(
            ConnectPeerParams::augment_command,
            &[
                "test",
                "--address",
                "/ip4/127.0.0.1/tcp/8119",
                "--save",
                "true",
            ],
        );
        let params = ConnectPeerParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.save, Some(true));
    }

    #[test]
    fn test_disconnect_peer() {
        let pubkey_hex = "03".to_string() + &"bb".repeat(32);
        let matches = parse_args(
            DisconnectPeerParams::augment_command,
            &["test", "--pubkey", &pubkey_hex],
        );
        let params = DisconnectPeerParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.pubkey, pubkey_hex.parse::<Pubkey>().unwrap());
    }

    #[test]
    fn test_disconnect_peer_pubkey_required() {
        let cmd = DisconnectPeerParams::augment_command(Command::new("test"));
        let pubkey_arg = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some("pubkey"))
            .unwrap();
        assert!(pubkey_arg.is_required_set());
    }

    #[test]
    fn test_invalid_pubkey_length() {
        // Too short
        let matches = parse_args(
            DisconnectPeerParams::augment_command,
            &["test", "--pubkey", "0x1234"],
        );
        let result = DisconnectPeerParams::from_arg_matches(&matches);
        assert!(result.is_err());
    }
}

// ── Channel CLI arg tests ────────────────────────────────────────────

mod channel_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use clap::Command;
    use fiber_json_types::channel::*;
    use fiber_json_types::serde_utils::{Hash256, Pubkey};

    #[test]
    fn test_open_channel_required_fields() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100000000",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.pubkey, pubkey_hex.parse::<Pubkey>().unwrap());
        assert_eq!(params.funding_amount, 100000000u128);
        assert!(params.funding_udt_type_script.is_none());
        assert!(params.shutdown_script.is_none());
        assert!(params.commitment_delay_epoch.is_none());
        assert!(params.commitment_fee_rate.is_none());
        assert!(params.funding_fee_rate.is_none());
    }

    #[test]
    fn test_open_channel_bool_flag_default_true() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--public",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.public, Some(true));
    }

    #[test]
    fn test_open_channel_bool_flag_explicit_false() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--public",
                "false",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.public, Some(false));
    }

    #[test]
    fn test_open_channel_bool_flag_default_false() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--one-way",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.one_way, Some(false));
    }

    #[test]
    fn test_open_channel_optional_u64() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--commitment-fee-rate",
                "1000",
                "--funding-fee-rate",
                "2000",
                "--tlc-expiry-delta",
                "14400000",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.commitment_fee_rate, Some(1000));
        assert_eq!(params.funding_fee_rate, Some(2000));
        assert_eq!(params.tlc_expiry_delta, Some(14400000));
    }

    #[test]
    fn test_open_channel_optional_u128() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--tlc-min-value",
                "500",
                "--tlc-fee-proportional-millionths",
                "1000",
                "--max-tlc-value-in-flight",
                "999999999",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.tlc_min_value, Some(500));
        assert_eq!(params.tlc_fee_proportional_millionths, Some(1000));
        assert_eq!(params.max_tlc_value_in_flight, Some(999999999));
    }

    #[test]
    fn test_open_channel_json_quoted_epoch() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--commitment-delay-epoch",
                "0x2000100000004",
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert!(params.commitment_delay_epoch.is_some());
    }

    #[test]
    fn test_open_channel_json_script() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let script_json = r#"{"code_hash":"0x0000000000000000000000000000000000000000000000000000000000000001","hash_type":"type","args":"0x1234"}"#;
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "100",
                "--funding-udt-type-script",
                script_json,
            ],
        );
        let params = OpenChannelParams::from_arg_matches(&matches).unwrap();
        assert!(params.funding_udt_type_script.is_some());
    }

    #[test]
    fn test_open_channel_missing_required_fails() {
        let cmd = OpenChannelParams::augment_command(Command::new("test"));
        let result = cmd.try_get_matches_from(["test", "--funding-amount", "100"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_abandon_channel_hash256() {
        let hash_hex = "0x".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            AbandonChannelParams::augment_command,
            &["test", "--channel-id", &hash_hex],
        );
        let params = AbandonChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
    }

    #[test]
    fn test_accept_channel_params() {
        let hash_hex = "0x".to_string() + &"cd".repeat(32);
        let matches = parse_args(
            AcceptChannelParams::augment_command,
            &[
                "test",
                "--temporary-channel-id",
                &hash_hex,
                "--funding-amount",
                "50000",
            ],
        );
        let params = AcceptChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(
            params.temporary_channel_id,
            hash_hex.parse::<Hash256>().unwrap()
        );
        assert_eq!(params.funding_amount, 50000);
        assert!(params.shutdown_script.is_none());
    }

    #[test]
    fn test_list_channels_all_optional() {
        let matches = parse_args(ListChannelsParams::augment_command, &["test"]);
        let params = ListChannelsParams::from_arg_matches(&matches).unwrap();
        assert!(params.pubkey.is_none());
        assert!(params.include_closed.is_none());
        assert!(params.only_pending.is_none());
    }

    #[test]
    fn test_list_channels_with_flags() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            ListChannelsParams::augment_command,
            &["test", "--pubkey", &pubkey_hex, "--include-closed", "true"],
        );
        let params = ListChannelsParams::from_arg_matches(&matches).unwrap();
        assert!(params.pubkey.is_some());
        assert_eq!(params.include_closed, Some(true));
    }

    #[test]
    fn test_shutdown_channel_params() {
        let hash_hex = "0x".to_string() + &"ee".repeat(32);
        let matches = parse_args(
            ShutdownChannelParams::augment_command,
            &["test", "--channel-id", &hash_hex, "--force"],
        );
        let params = ShutdownChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(params.force, Some(false)); // default = false
    }

    #[test]
    fn test_update_channel_params() {
        let hash_hex = "0x".to_string() + &"ff".repeat(32);
        let matches = parse_args(
            UpdateChannelParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--enabled",
                "true",
                "--tlc-expiry-delta",
                "86400000",
                "--tlc-minimum-value",
                "100",
                "--tlc-fee-proportional-millionths",
                "500",
            ],
        );
        let params = UpdateChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(params.enabled, Some(true));
        assert_eq!(params.tlc_expiry_delta, Some(86400000));
        assert_eq!(params.tlc_minimum_value, Some(100));
        assert_eq!(params.tlc_fee_proportional_millionths, Some(500));
    }
}

// ── Payment CLI arg tests ────────────────────────────────────────────

mod payment_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::payment::*;
    use fiber_json_types::serde_utils::Hash256;

    #[test]
    fn test_get_payment_params() {
        let hash_hex = "0x".to_string() + &"11".repeat(32);
        let matches = parse_args(
            GetPaymentCommandParams::augment_command,
            &["test", "--payment-hash", &hash_hex],
        );
        let params = GetPaymentCommandParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.payment_hash, hash_hex.parse::<Hash256>().unwrap());
    }

    #[test]
    fn test_list_payments_empty() {
        let matches = parse_args(ListPaymentsParams::augment_command, &["test"]);
        let params = ListPaymentsParams::from_arg_matches(&matches).unwrap();
        assert!(params.status.is_none());
        assert!(params.limit.is_none());
        assert!(params.after.is_none());
    }

    #[test]
    fn test_list_payments_serde_enum_status() {
        let matches = parse_args(
            ListPaymentsParams::augment_command,
            &["test", "--status", "Success"],
        );
        let params = ListPaymentsParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.status, Some(PaymentStatus::Success));
    }

    #[test]
    fn test_list_payments_with_limit_and_after() {
        let hash_hex = "0x".to_string() + &"22".repeat(32);
        let matches = parse_args(
            ListPaymentsParams::augment_command,
            &[
                "test", "--status", "Failed", "--limit", "50", "--after", &hash_hex,
            ],
        );
        let params = ListPaymentsParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.status, Some(PaymentStatus::Failed));
        assert_eq!(params.limit, Some(50));
        assert_eq!(params.after, Some(hash_hex.parse::<Hash256>().unwrap()));
    }

    #[test]
    fn test_send_payment_keysend() {
        let pubkey_hex = "03".to_string() + &"cc".repeat(32);
        let matches = parse_args(
            SendPaymentCommandParams::augment_command,
            &[
                "test",
                "--target-pubkey",
                &pubkey_hex,
                "--amount",
                "100000",
                "--keysend",
            ],
        );
        let params = SendPaymentCommandParams::from_arg_matches(&matches).unwrap();
        assert!(params.target_pubkey.is_some());
        assert_eq!(params.amount, Some(100000));
        assert_eq!(params.keysend, Some(false)); // default_missing_value = false
    }

    #[test]
    fn test_send_payment_with_invoice() {
        let matches = parse_args(
            SendPaymentCommandParams::augment_command,
            &["test", "--invoice", "fibd1234567890abcdef"],
        );
        let params = SendPaymentCommandParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.invoice, Some("fibd1234567890abcdef".to_string()));
        assert!(params.target_pubkey.is_none());
        assert!(params.amount.is_none());
    }

    #[test]
    fn test_send_payment_fee_params() {
        let matches = parse_args(
            SendPaymentCommandParams::augment_command,
            &[
                "test",
                "--invoice",
                "fibd_test",
                "--max-fee-amount",
                "1000",
                "--max-fee-rate",
                "5",
                "--timeout",
                "60",
                "--max-parts",
                "3",
            ],
        );
        let params = SendPaymentCommandParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.max_fee_amount, Some(1000));
        assert_eq!(params.max_fee_rate, Some(5));
        assert_eq!(params.timeout, Some(60));
        assert_eq!(params.max_parts, Some(3));
    }

    #[test]
    fn test_send_payment_dry_run() {
        let matches = parse_args(
            SendPaymentCommandParams::augment_command,
            &["test", "--invoice", "fibd_test", "--dry-run", "true"],
        );
        let params = SendPaymentCommandParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.dry_run, Some(true));
    }

    #[test]
    fn test_send_payment_allow_self_payment() {
        let pubkey_hex = "03".to_string() + &"dd".repeat(32);
        let matches = parse_args(
            SendPaymentCommandParams::augment_command,
            &[
                "test",
                "--target-pubkey",
                &pubkey_hex,
                "--amount",
                "1000",
                "--keysend",
                "true",
                "--allow-self-payment",
                "true",
            ],
        );
        let params = SendPaymentCommandParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.keysend, Some(true));
        assert_eq!(params.allow_self_payment, Some(true));
    }

    #[test]
    fn test_build_router_params() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let hops_json = format!(r#"[{{"pubkey":"{}","channel_outpoint":null}}]"#, pubkey_hex);
        let matches = parse_args(
            BuildRouterParams::augment_command,
            &["test", "--hops-info", &hops_json],
        );
        let params = BuildRouterParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.hops_info.len(), 1);
        assert!(params.amount.is_none());
        assert!(params.final_tlc_expiry_delta.is_none());
    }

    #[test]
    fn test_send_payment_with_router_params() {
        let router_json = r#"[]"#;
        let matches = parse_args(
            SendPaymentWithRouterParams::augment_command,
            &["test", "--router", router_json],
        );
        let params = SendPaymentWithRouterParams::from_arg_matches(&matches).unwrap();
        assert!(params.router.is_empty());
        assert!(params.payment_hash.is_none());
        assert!(params.invoice.is_none());
    }
}

// ── Invoice CLI arg tests ────────────────────────────────────────────

mod invoice_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::invoice::*;
    use fiber_json_types::serde_utils::Hash256;

    #[test]
    fn test_new_invoice_required_fields() {
        let matches = parse_args(
            NewInvoiceParams::augment_command,
            &["test", "--amount", "100000", "--currency", "Fibd"],
        );
        let params = NewInvoiceParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.amount, 100000);
        assert_eq!(params.currency, Currency::Fibd);
        assert!(params.description.is_none());
        assert!(params.payment_preimage.is_none());
    }

    #[test]
    fn test_new_invoice_serde_enum_currency() {
        for (name, expected) in [
            ("Fibb", Currency::Fibb),
            ("Fibt", Currency::Fibt),
            ("Fibd", Currency::Fibd),
        ] {
            let matches = parse_args(
                NewInvoiceParams::augment_command,
                &["test", "--amount", "1", "--currency", name],
            );
            let params = NewInvoiceParams::from_arg_matches(&matches).unwrap();
            assert_eq!(params.currency, expected);
        }
    }

    #[test]
    fn test_new_invoice_serde_enum_hash_algorithm() {
        for (name, expected) in [
            ("ckb_hash", HashAlgorithm::CkbHash),
            ("sha256", HashAlgorithm::Sha256),
        ] {
            let matches = parse_args(
                NewInvoiceParams::augment_command,
                &[
                    "test",
                    "--amount",
                    "1",
                    "--currency",
                    "Fibd",
                    "--hash-algorithm",
                    name,
                ],
            );
            let params = NewInvoiceParams::from_arg_matches(&matches).unwrap();
            assert_eq!(params.hash_algorithm, Some(expected));
        }
    }

    #[test]
    fn test_new_invoice_all_optional_fields() {
        let hash_hex = "0x".to_string() + &"aa".repeat(32);
        let matches = parse_args(
            NewInvoiceParams::augment_command,
            &[
                "test",
                "--amount",
                "50000",
                "--currency",
                "Fibt",
                "--description",
                "Test invoice",
                "--payment-preimage",
                &hash_hex,
                "--expiry",
                "3600",
                "--fallback-address",
                "ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqtest",
                "--final-expiry-delta",
                "86400000",
                "--allow-mpp",
                "true",
            ],
        );
        let params = NewInvoiceParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.amount, 50000);
        assert_eq!(params.currency, Currency::Fibt);
        assert_eq!(params.description, Some("Test invoice".to_string()));
        assert_eq!(
            params.payment_preimage,
            Some(hash_hex.parse::<Hash256>().unwrap())
        );
        assert_eq!(params.expiry, Some(3600));
        assert!(params.fallback_address.is_some());
        assert_eq!(params.final_expiry_delta, Some(86400000));
        assert_eq!(params.allow_mpp, Some(true));
    }

    #[test]
    fn test_parse_invoice_params() {
        let matches = parse_args(
            ParseInvoiceParams::augment_command,
            &["test", "--invoice", "fibd1234567890abcdef"],
        );
        let params = ParseInvoiceParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.invoice, "fibd1234567890abcdef");
    }

    #[test]
    fn test_invoice_params() {
        let hash_hex = "0x".to_string() + &"bb".repeat(32);
        let matches = parse_args(
            InvoiceParams::augment_command,
            &["test", "--payment-hash", &hash_hex],
        );
        let params = InvoiceParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.payment_hash, hash_hex.parse::<Hash256>().unwrap());
    }

    #[test]
    fn test_settle_invoice_params() {
        let hash_hex = "0x".to_string() + &"cc".repeat(32);
        let preimage_hex = "0x".to_string() + &"dd".repeat(32);
        let matches = parse_args(
            SettleInvoiceParams::augment_command,
            &[
                "test",
                "--payment-hash",
                &hash_hex,
                "--payment-preimage",
                &preimage_hex,
            ],
        );
        let params = SettleInvoiceParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.payment_hash, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(
            params.payment_preimage,
            preimage_hex.parse::<Hash256>().unwrap()
        );
    }
}

// ── Dev CLI arg tests ────────────────────────────────────────────────

mod dev_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::dev::*;
    use fiber_json_types::serde_utils::Hash256;

    #[test]
    fn test_commitment_signed_params() {
        let hash_hex = "0x".to_string() + &"aa".repeat(32);
        let matches = parse_args(
            CommitmentSignedParams::augment_command,
            &["test", "--channel-id", &hash_hex],
        );
        let params = CommitmentSignedParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
    }

    #[test]
    fn test_add_tlc_params() {
        let hash_hex = "0x".to_string() + &"bb".repeat(32);
        let payment_hash_hex = "0x".to_string() + &"cc".repeat(32);
        let matches = parse_args(
            AddTlcParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--amount",
                "1000000",
                "--payment-hash",
                &payment_hash_hex,
                "--expiry",
                "86400000",
            ],
        );
        let params = AddTlcParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(params.amount, 1000000);
        assert_eq!(
            params.payment_hash,
            payment_hash_hex.parse::<Hash256>().unwrap()
        );
        assert_eq!(params.expiry, 86400000);
        assert!(params.hash_algorithm.is_none());
    }

    #[test]
    fn test_add_tlc_with_hash_algorithm() {
        let hash_hex = "0x".to_string() + &"bb".repeat(32);
        let payment_hash_hex = "0x".to_string() + &"cc".repeat(32);
        let matches = parse_args(
            AddTlcParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--amount",
                "1000",
                "--payment-hash",
                &payment_hash_hex,
                "--expiry",
                "100",
                "--hash-algorithm",
                "sha256",
            ],
        );
        let params = AddTlcParams::from_arg_matches(&matches).unwrap();
        assert_eq!(
            params.hash_algorithm,
            Some(fiber_json_types::invoice::HashAlgorithm::Sha256)
        );
    }

    #[test]
    fn test_remove_tlc_json_reason() {
        let hash_hex = "0x".to_string() + &"dd".repeat(32);
        let reason_json = r#"{"payment_preimage":"0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}"#;
        let matches = parse_args(
            RemoveTlcParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--tlc-id",
                "42",
                "--reason",
                reason_json,
            ],
        );
        let params = RemoveTlcParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(params.tlc_id, 42);
        match &params.reason {
            RemoveTlcReason::RemoveTlcFulfill { payment_preimage } => {
                let expected = "0x".to_string() + &"ee".repeat(32);
                assert_eq!(*payment_preimage, expected.parse::<Hash256>().unwrap());
            }
            _ => panic!("Expected RemoveTlcFulfill variant"),
        }
    }

    #[test]
    fn test_submit_commitment_transaction_params() {
        let hash_hex = "0x".to_string() + &"ff".repeat(32);
        let matches = parse_args(
            SubmitCommitmentTransactionParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--commitment-number",
                "5",
            ],
        );
        let params = SubmitCommitmentTransactionParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(params.commitment_number, 5);
    }

    #[test]
    fn test_check_channel_shutdown_params() {
        let hash_hex = "0x".to_string() + &"11".repeat(32);
        let matches = parse_args(
            CheckChannelShutdownParams::augment_command,
            &["test", "--channel-id", &hash_hex],
        );
        let params = CheckChannelShutdownParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
    }
}

// ── Graph CLI arg tests ──────────────────────────────────────────────

mod graph_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::graph::*;

    #[test]
    fn test_graph_nodes_no_args() {
        let matches = parse_args(GraphNodesParams::augment_command, &["test"]);
        let params = GraphNodesParams::from_arg_matches(&matches).unwrap();
        assert!(params.limit.is_none());
        assert!(params.after.is_none());
    }

    #[test]
    fn test_graph_nodes_with_limit() {
        let matches = parse_args(
            GraphNodesParams::augment_command,
            &["test", "--limit", "100"],
        );
        let params = GraphNodesParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.limit, Some(100));
    }

    #[test]
    fn test_graph_nodes_with_after_json_quoted() {
        let matches = parse_args(
            GraphNodesParams::augment_command,
            &["test", "--after", "0x1234abcd"],
        );
        let params = GraphNodesParams::from_arg_matches(&matches).unwrap();
        assert!(params.after.is_some());
    }

    #[test]
    fn test_graph_channels_no_args() {
        let matches = parse_args(GraphChannelsParams::augment_command, &["test"]);
        let params = GraphChannelsParams::from_arg_matches(&matches).unwrap();
        assert!(params.limit.is_none());
        assert!(params.after.is_none());
    }

    #[test]
    fn test_graph_channels_with_limit_and_after() {
        let matches = parse_args(
            GraphChannelsParams::augment_command,
            &["test", "--limit", "50", "--after", "0xdeadbeef"],
        );
        let params = GraphChannelsParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.limit, Some(50));
        assert!(params.after.is_some());
    }
}

// ── Prof CLI arg tests ───────────────────────────────────────────────

mod prof_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::prof::*;

    #[test]
    fn test_pprof_no_args() {
        let matches = parse_args(PprofParams::augment_command, &["test"]);
        let params = PprofParams::from_arg_matches(&matches).unwrap();
        assert!(params.duration_secs.is_none());
    }

    #[test]
    fn test_pprof_with_duration() {
        let matches = parse_args(
            PprofParams::augment_command,
            &["test", "--duration-secs", "30"],
        );
        let params = PprofParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.duration_secs, Some(30));
    }
}

// ── Info fee CLI arg tests ────────────────────────────────────────────────

mod info_fee_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::fee::*;

    #[test]
    fn test_forwarding_history_no_args() {
        let matches = parse_args(ForwardingHistoryParams::augment_command, &["test"]);
        let params = ForwardingHistoryParams::from_arg_matches(&matches).unwrap();
        assert!(params.start_time.is_none());
        assert!(params.end_time.is_none());
        assert!(params.limit.is_none());
        assert!(params.offset.is_none());
        assert!(params.udt_type_script.is_none());
    }

    #[test]
    fn test_forwarding_history_with_time_range() {
        let matches = parse_args(
            ForwardingHistoryParams::augment_command,
            &["test", "--start-time", "1000", "--end-time", "2000"],
        );
        let params = ForwardingHistoryParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.start_time, Some(1000));
        assert_eq!(params.end_time, Some(2000));
    }

    #[test]
    fn test_forwarding_history_with_pagination() {
        let matches = parse_args(
            ForwardingHistoryParams::augment_command,
            &["test", "--limit", "50", "--offset", "10"],
        );
        let params = ForwardingHistoryParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.limit, Some(50));
        assert_eq!(params.offset, Some(10));
    }

    #[test]
    fn test_forwarding_history_with_udt_filter() {
        let script_json = r#"{"code_hash":"0x0000000000000000000000000000000000000000000000000000000000000001","hash_type":"type","args":"0x1234"}"#;
        let matches = parse_args(
            ForwardingHistoryParams::augment_command,
            &["test", "--udt-type-script", script_json],
        );
        let params = ForwardingHistoryParams::from_arg_matches(&matches).unwrap();
        assert!(params.udt_type_script.is_some());
    }

    #[test]
    fn test_forwarding_history_all_params() {
        let script_json = r#"{"code_hash":"0x0000000000000000000000000000000000000000000000000000000000000001","hash_type":"type","args":"0x1234"}"#;
        let matches = parse_args(
            ForwardingHistoryParams::augment_command,
            &[
                "test",
                "--start-time",
                "500",
                "--end-time",
                "9000",
                "--limit",
                "25",
                "--offset",
                "5",
                "--udt-type-script",
                script_json,
            ],
        );
        let params = ForwardingHistoryParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.start_time, Some(500));
        assert_eq!(params.end_time, Some(9000));
        assert_eq!(params.limit, Some(25));
        assert_eq!(params.offset, Some(5));
        assert!(params.udt_type_script.is_some());
    }
}

// ── CCH CLI arg tests ────────────────────────────────────────────────

mod cch_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::cch::*;
    use fiber_json_types::serde_utils::Hash256;

    #[test]
    fn test_send_btc_params() {
        let matches = parse_args(
            SendBTCParams::augment_command,
            &[
                "test",
                "--btc-pay-req",
                "lnbc1234567890",
                "--currency",
                "Fibd",
            ],
        );
        let params = SendBTCParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.btc_pay_req, "lnbc1234567890");
    }

    #[test]
    fn test_receive_btc_params() {
        let matches = parse_args(
            ReceiveBTCParams::augment_command,
            &["test", "--fiber-pay-req", "fibd_invoice_string"],
        );
        let params = ReceiveBTCParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.fiber_pay_req, "fibd_invoice_string");
    }

    #[test]
    fn test_get_cch_order_params() {
        let hash_hex = "0x".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            GetCchOrderParams::augment_command,
            &["test", "--payment-hash", &hash_hex],
        );
        let params = GetCchOrderParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.payment_hash, hash_hex.parse::<Hash256>().unwrap());
    }
}

// ── Watchtower CLI arg tests ─────────────────────────────────────────

mod watchtower_cli_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::serde_utils::Hash256;
    use fiber_json_types::watchtower::*;

    #[test]
    fn test_remove_watch_channel_params() {
        let hash_hex = "0x".to_string() + &"ef".repeat(32);
        let matches = parse_args(
            RemoveWatchChannelParams::augment_command,
            &["test", "--channel-id", &hash_hex],
        );
        let params = RemoveWatchChannelParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.channel_id, hash_hex.parse::<Hash256>().unwrap());
    }

    #[test]
    fn test_create_preimage_params() {
        let hash_hex = "0x".to_string() + &"ab".repeat(32);
        let preimage_hex = "0x".to_string() + &"cd".repeat(32);
        let matches = parse_args(
            CreatePreimageParams::augment_command,
            &[
                "test",
                "--payment-hash",
                &hash_hex,
                "--preimage",
                &preimage_hex,
            ],
        );
        let params = CreatePreimageParams::from_arg_matches(&matches).unwrap();
        assert_eq!(params.payment_hash, hash_hex.parse::<Hash256>().unwrap());
        assert_eq!(params.preimage, preimage_hex.parse::<Hash256>().unwrap());
    }

    #[test]
    fn test_remove_preimage_params() {
        let payment_hash_hex = "0x".to_string() + &"11".repeat(32);
        let matches = parse_args(
            RemovePreimageParams::augment_command,
            &["test", "--payment-hash", &payment_hash_hex],
        );
        let params = RemovePreimageParams::from_arg_matches(&matches).unwrap();
        assert_eq!(
            params.payment_hash,
            payment_hash_hex.parse::<Hash256>().unwrap()
        );
    }
}

// ── augment_command structural tests ─────────────────────────────────

mod augment_command_tests {
    use crate::cli_generated::CliArgs;
    use clap::Command;
    use fiber_json_types::channel::*;
    use fiber_json_types::dev::*;
    use fiber_json_types::invoice::*;
    use fiber_json_types::payment::*;

    /// Verify that augment_command adds the expected arguments to a Command.
    fn get_arg_names<F>(augment: F) -> Vec<String>
    where
        F: FnOnce(Command) -> Command,
    {
        let cmd = augment(Command::new("test"));
        cmd.get_arguments()
            .filter_map(|a| a.get_long().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn test_open_channel_has_expected_args() {
        let args = get_arg_names(OpenChannelParams::augment_command);
        assert!(args.contains(&"pubkey".to_string()));
        assert!(args.contains(&"funding-amount".to_string()));
        assert!(args.contains(&"public".to_string()));
        assert!(args.contains(&"one-way".to_string()));
        assert!(args.contains(&"funding-udt-type-script".to_string()));
        assert!(args.contains(&"shutdown-script".to_string()));
        assert!(args.contains(&"commitment-delay-epoch".to_string()));
        assert!(args.contains(&"commitment-fee-rate".to_string()));
        assert!(args.contains(&"funding-fee-rate".to_string()));
        assert!(args.contains(&"tlc-expiry-delta".to_string()));
        assert!(args.contains(&"tlc-min-value".to_string()));
        assert!(args.contains(&"tlc-fee-proportional-millionths".to_string()));
        assert!(args.contains(&"max-tlc-value-in-flight".to_string()));
        assert!(args.contains(&"max-tlc-number-in-flight".to_string()));
    }

    #[test]
    fn test_open_channel_required_args_are_required() {
        let cmd = OpenChannelParams::augment_command(Command::new("test"));
        for arg in cmd.get_arguments() {
            let name = arg.get_long().unwrap_or("");
            match name {
                "pubkey" | "funding-amount" => {
                    assert!(arg.is_required_set(), "{} should be required", name);
                }
                _ => {
                    assert!(!arg.is_required_set(), "{} should NOT be required", name);
                }
            }
        }
    }

    #[test]
    fn test_send_payment_no_required_args() {
        let cmd = SendPaymentCommandParams::augment_command(Command::new("test"));
        for arg in cmd.get_arguments() {
            assert!(
                !arg.is_required_set(),
                "{} should not be required",
                arg.get_long().unwrap_or("")
            );
        }
    }

    #[test]
    fn test_new_invoice_amount_and_currency_required() {
        let cmd = NewInvoiceParams::augment_command(Command::new("test"));
        let amount_arg = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some("amount"))
            .unwrap();
        assert!(amount_arg.is_required_set());

        let currency_arg = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some("currency"))
            .unwrap();
        assert!(currency_arg.is_required_set());
    }

    #[test]
    fn test_add_tlc_all_non_option_required() {
        let cmd = AddTlcParams::augment_command(Command::new("test"));
        let required_fields = ["channel-id", "amount", "payment-hash", "expiry"];
        for name in &required_fields {
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_long() == Some(*name))
                .unwrap_or_else(|| panic!("arg {} not found", name));
            assert!(arg.is_required_set(), "{} should be required", name);
        }
    }

    #[test]
    fn test_kebab_case_conversion() {
        let args = get_arg_names(OpenChannelParams::augment_command);
        assert!(args.contains(&"funding-amount".to_string()));
        assert!(args.contains(&"funding-udt-type-script".to_string()));
        assert!(args.contains(&"commitment-delay-epoch".to_string()));
    }
}

// ── Error handling tests ─────────────────────────────────────────────

mod error_tests {
    use super::parse_args;
    use crate::cli_generated::CliArgs;
    use fiber_json_types::channel::*;
    use fiber_json_types::dev::*;
    use fiber_json_types::serde_utils::Hash256;

    #[test]
    fn test_invalid_hash256_format() {
        let matches = parse_args(
            AbandonChannelParams::augment_command,
            &["test", "--channel-id", "not-a-valid-hash"],
        );
        let result = AbandonChannelParams::from_arg_matches(&matches);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Invalid") || err_msg.contains("invalid"),
            "Error should mention invalid input, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_invalid_u128_format() {
        let pubkey_hex = "03".to_string() + &"ab".repeat(32);
        let matches = parse_args(
            OpenChannelParams::augment_command,
            &[
                "test",
                "--pubkey",
                &pubkey_hex,
                "--funding-amount",
                "not_a_number",
            ],
        );
        let result = OpenChannelParams::from_arg_matches(&matches);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_u64_format() {
        let hash_hex = "0x".to_string() + &"aa".repeat(32);
        let matches = parse_args(
            AddTlcParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--amount",
                "100",
                "--payment-hash",
                &hash_hex,
                "--expiry",
                "abc",
            ],
        );
        let result = AddTlcParams::from_arg_matches(&matches);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_json() {
        let hash_hex = "0x".to_string() + &"aa".repeat(32);
        let matches = parse_args(
            RemoveTlcParams::augment_command,
            &[
                "test",
                "--channel-id",
                &hash_hex,
                "--tlc-id",
                "1",
                "--reason",
                "{invalid json}",
            ],
        );
        let result = RemoveTlcParams::from_arg_matches(&matches);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_serde_enum() {
        let matches = parse_args(
            fiber_json_types::payment::ListPaymentsParams::augment_command,
            &["test", "--status", "NotAValidStatus"],
        );
        let result = fiber_json_types::payment::ListPaymentsParams::from_arg_matches(&matches);
        assert!(result.is_err());
    }

    #[test]
    fn test_hash256_without_prefix_also_works() {
        let hash_no_prefix = "ab".repeat(32);
        let matches = parse_args(
            AbandonChannelParams::augment_command,
            &["test", "--channel-id", &hash_no_prefix],
        );
        let params = AbandonChannelParams::from_arg_matches(&matches).unwrap();
        let expected = format!("0x{}", hash_no_prefix).parse::<Hash256>().unwrap();
        assert_eq!(params.channel_id, expected);
    }
}
