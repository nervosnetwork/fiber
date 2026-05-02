/// Helper to write a simple config.toml in the temporary HOME directory
pub fn write_config(tmp: &tempfile::TempDir, url: &str, auth_token: &str) -> std::io::Result<()> {
    let config_dir = tmp.path().join(".fnn");
    std::fs::create_dir_all(&config_dir)?;
    let content = format!("url = \"{}\"\nauth_token = \"{}\"\n", url, auth_token);
    std::fs::write(config_dir.join("config.toml"), content)?;
    Ok(())
}

/// Build a minimal, valid `node_info` JSON-RPC response string.
pub fn make_node_info_response(hex_pair: &str) -> String {
    let pubkey = format!("02{}", hex_pair.repeat(32));
    let v = serde_json::json!({
        "jsonrpc": "2.0",
        "result": {
            "version": "0.1.0",
            "commit_hash": "deadbeef",
            "pubkey": pubkey,
            "features": [],
            "node_name": null,
            "addresses": [],
            "chain_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "open_channel_auto_accept_min_ckb_funding_amount": "0x0",
            "auto_accept_channel_ckb_funding_amount": "0x0",
            "default_funding_lock_script": {
                "code_hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "hash_type": "type",
                "args": "0x00"
            },
            "tlc_expiry_delta": "0x0",
            "tlc_min_value": "0x0",
            "tlc_fee_proportional_millionths": "0x0",
            "channel_count": "0x0",
            "pending_channel_count": "0x0",
            "peers_count": "0x0",
            "udt_cfg_infos": []
        },
        "id": 1
    });
    v.to_string()
}
