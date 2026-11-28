use super::*;

#[test]
fn test_new_auto_prepends_http() {
    let client = RpcClient::new("127.0.0.1:8227", false, None).unwrap();
    assert_eq!(client.url(), "http://127.0.0.1:8227");
}

#[test]
fn test_new_auto_prepends_http_for_remote_without_auth() {
    let client = RpcClient::new("example.com:8227", false, None).unwrap();
    assert_eq!(client.url(), "http://example.com:8227");
}

#[test]
fn test_new_rejects_implicit_http_for_remote_with_auth() {
    let result = RpcClient::new(
        "example.com:8227",
        false,
        Some("my-secret-token".to_string()),
    );
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("implicit plaintext HTTP"),
        "expected 'implicit plaintext HTTP' in error: {}",
        err
    );
    assert!(
        err.contains("https://example.com:8227/"),
        "expected HTTPS suggestion in error: {}",
        err
    );
}

#[test]
fn test_new_accepts_implicit_http_for_loopback_with_auth() {
    let client = RpcClient::new(
        "127.8.9.10:8227",
        false,
        Some("my-secret-token".to_string()),
    )
    .unwrap();
    assert_eq!(client.url(), "http://127.8.9.10:8227");

    let client =
        RpcClient::new("localhost:8227", false, Some("my-secret-token".to_string())).unwrap();
    assert_eq!(client.url(), "http://localhost:8227");

    let client = RpcClient::new("[::1]:8227", false, Some("my-secret-token".to_string())).unwrap();
    assert_eq!(client.url(), "http://[::1]:8227");
}

#[test]
fn test_new_preserves_http_scheme() {
    let client = RpcClient::new("http://example.com:8227", false, None).unwrap();
    assert_eq!(client.url(), "http://example.com:8227");
}

#[test]
fn test_new_preserves_https_scheme() {
    let client = RpcClient::new("https://example.com:8227", false, None).unwrap();
    assert_eq!(client.url(), "https://example.com:8227");
}

#[test]
fn test_new_accepts_https_for_remote_with_auth() {
    let client = RpcClient::new(
        "https://example.com:8227",
        false,
        Some("my-secret-token".to_string()),
    )
    .unwrap();
    assert_eq!(client.url(), "https://example.com:8227");
}

#[test]
fn test_new_accepts_explicit_http_for_remote_with_auth() {
    let client = RpcClient::new(
        "http://example.com:8227",
        false,
        Some("my-secret-token".to_string()),
    )
    .unwrap();
    assert_eq!(client.url(), "http://example.com:8227");
}

#[test]
fn test_raw_data_flag() {
    let client = RpcClient::new("http://localhost", true, None).unwrap();
    assert!(client.raw_data());

    let client = RpcClient::new("http://localhost", false, None).unwrap();
    assert!(!client.raw_data());
}

#[test]
fn test_has_auth_with_token() {
    let client = RpcClient::new(
        "http://localhost",
        false,
        Some("my-secret-token".to_string()),
    )
    .unwrap();
    assert!(client.auth_token().is_some());
    assert_eq!(client.auth_token().unwrap(), "my-secret-token");
}

#[test]
fn test_has_auth_without_token() {
    let client = RpcClient::new("http://localhost", false, None).unwrap();
    assert!(client.auth_token().is_none());
}

#[test]
fn test_clone_preserves_auth() {
    let client = RpcClient::new(
        "http://localhost",
        false,
        Some("my-secret-token".to_string()),
    )
    .unwrap();
    let cloned = client.clone();
    assert!(cloned.auth_token().is_some());
    assert_eq!(cloned.url(), client.url());
    assert_eq!(cloned.raw_data(), client.raw_data());
}

#[test]
fn test_auth_token_trimmed() {
    let client = RpcClient::new(
        "http://localhost",
        false,
        Some("  my-token  \n".to_string()),
    )
    .unwrap();
    assert_eq!(client.auth_token().unwrap(), "my-token");
}

#[test]
fn test_auth_token_strips_internal_whitespace() {
    // Simulates token pasted with a line break in the middle
    let client = RpcClient::new(
        "http://localhost",
        false,
        Some("abc123\ndef456".to_string()),
    )
    .unwrap();
    assert_eq!(client.auth_token().unwrap(), "abc123def456");
}

#[test]
fn test_auth_token_empty_after_trim() {
    let client = RpcClient::new("http://localhost", false, Some("  \n".to_string())).unwrap();
    assert!(client.auth_token().is_none());
}

// ── URL validation tests ─────────────────────────────────────────────

#[test]
fn test_duplicate_http_scheme_rejected() {
    let result = RpcClient::new("http://http://127.0.0.1:8227", false, None);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("duplicate scheme"),
        "expected 'duplicate scheme' in error: {}",
        err
    );
    assert!(
        err.contains("http://127.0.0.1:8227"),
        "expected suggestion in error: {}",
        err
    );
}

#[test]
fn test_duplicate_https_scheme_rejected() {
    let result = RpcClient::new("https://http://127.0.0.1:8227", false, None);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("duplicate scheme"));
}

#[test]
fn test_duplicate_scheme_without_initial_scheme_rejected() {
    // User types "http://127.0.0.1:8227" but accidentally gets auto-prepended
    // This should still work since "http://http://..." is caught
    let result = RpcClient::new("http://https://example.com", false, None);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("duplicate scheme"));
}

#[test]
fn test_valid_url_accepted() {
    let client = RpcClient::new("http://127.0.0.1:8227", false, None).unwrap();
    assert_eq!(client.url(), "http://127.0.0.1:8227");
}

#[test]
fn test_valid_localhost_url_accepted() {
    let client = RpcClient::new("http://localhost:8227", false, None).unwrap();
    assert_eq!(client.url(), "http://localhost:8227");
}
