use super::validate_watchtower_config;
use fnn::FiberConfig;

#[test]
fn validate_watchtower_config_rejects_lsp_with_disabled_built_in_watchtower() {
    let mut lsp_with_disabled_and_standalone = FiberConfig::default();
    lsp_with_disabled_and_standalone.disable_built_in_watchtower = Some(true);
    lsp_with_disabled_and_standalone.standalone_watchtower_rpc_url =
        Some("http://127.0.0.1:8227".to_string());
    let err = validate_watchtower_config(&lsp_with_disabled_and_standalone, true)
        .expect_err("LSP must reject disable_built_in_watchtower: true even with standalone URL");
    assert!(
        err.0.contains("LSP") && err.0.contains("built-in watchtower"),
        "unexpected error message: {}",
        err.0
    );

    let mut lsp_with_disabled_only = FiberConfig::default();
    lsp_with_disabled_only.disable_built_in_watchtower = Some(true);
    assert!(validate_watchtower_config(&lsp_with_disabled_only, true).is_err());

    // LSP with built-in watchtower enabled succeeds (with or without standalone URL).
    let lsp_default = FiberConfig::default();
    assert!(validate_watchtower_config(&lsp_default, true).is_ok());

    let mut lsp_explicit_enabled = FiberConfig::default();
    lsp_explicit_enabled.disable_built_in_watchtower = Some(false);
    lsp_explicit_enabled.standalone_watchtower_rpc_url = Some("http://127.0.0.1:8227".to_string());
    assert!(validate_watchtower_config(&lsp_explicit_enabled, true).is_ok());

    // Non-LSP with standalone watchtower and disable_built_in_watchtower: true still succeeds.
    assert!(validate_watchtower_config(&lsp_with_disabled_and_standalone, false).is_ok());

    // Non-LSP with neither standalone nor built-in watchtower fails.
    assert!(validate_watchtower_config(&lsp_with_disabled_only, false).is_err());
}
