use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;
use wiremock::matchers::{body_string_contains, header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

include!("common.rs");

#[tokio::test]
async fn auth_token_file_is_read_and_used() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    // Expect Authorization header from token file
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .and(header("authorization", "Bearer token-from-file"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(make_node_info_response("aa"), "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // write token file
    let token_file = tmp.path().join("token.txt");
    fs::write(&token_file, "token-from-file\n")?;

    // Run CLI with --auth-token-file and --url
    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--url")
        .arg(mock_server.uri())
        .arg("--auth-token-file")
        .arg(token_file)
        .arg("info")
        .assert()
        .success();

    Ok(())
}

#[tokio::test]
async fn raw_data_flag_prints_raw_json() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    let body = make_node_info_response("ab");
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.clone(), "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--url")
        .arg(mock_server.uri())
        .arg("--raw-data")
        .arg("info")
        .assert()
        .success()
        .stdout(predicate::str::contains("version").and(predicate::str::contains("0.1.0")));

    Ok(())
}

#[tokio::test]
async fn output_format_json_produces_json() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    let body = make_node_info_response("ac");
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.clone(), "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--url")
        .arg(mock_server.uri())
        .arg("--output-format")
        .arg("json")
        .arg("info")
        .assert()
        .success()
        .stdout(predicate::str::contains("version").and(predicate::str::contains("0.1.0")));

    Ok(())
}

#[tokio::test]
async fn rpc_error_results_in_nonzero_exit_and_error_message(
) -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    // Respond with JSON-RPC error
    let error_body = r#"{"jsonrpc":"2.0","error":{"code":123,"message":"boom"},"id":1}"#;
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(error_body, "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    let assert = cmd
        .env("HOME", tmp.path())
        .arg("--url")
        .arg(mock_server.uri())
        .arg("info")
        .assert();

    // Should fail and stderr contain the RPC error message
    assert
        .failure()
        .stderr(predicate::str::contains("RPC error").or(predicate::str::contains("boom")));

    Ok(())
}

#[tokio::test]
async fn config_flag_points_to_custom_config_path() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let custom_dir = tmp.path().join("myconf");
    std::fs::create_dir_all(&custom_dir)?;
    let config_path = custom_dir.join("cfg.toml");

    let mock_server = MockServer::start().await;
    let node_info = make_node_info_response("de");
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .respond_with(ResponseTemplate::new(200).set_body_raw(node_info, "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    // write custom config pointing to mock_server
    fs::write(&config_path, format!("url = \"{}\"\n", mock_server.uri()))?;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--config")
        .arg(config_path)
        .arg("info")
        .assert()
        .success();

    Ok(())
}

// helpers are included from common.rs

#[tokio::test]
async fn config_file_is_used_when_no_env_or_cli() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;

    // Start mock server which will act as the RPC endpoint configured in the file
    let mock_server = MockServer::start().await;

    let node_info_body = make_node_info_response("11");
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .and(header("authorization", "Bearer tok-from-config"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(node_info_body, "application/json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    write_config(&tmp, &mock_server.uri(), "tok-from-config")?;

    // Run the binary with HOME pointed to our temp dir
    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path()).arg("info").assert().success();

    Ok(())
}

#[tokio::test]
async fn env_overrides_config_file() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;

    let config_server = MockServer::start().await;
    let env_server = MockServer::start().await;

    // Config server should NOT receive requests; env server should
    let node_info_body_env = make_node_info_response("22");
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .and(header("authorization", "Bearer tok-from-env"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(node_info_body_env, "application/json"),
        )
        .expect(1)
        .mount(&env_server)
        .await;

    write_config(&tmp, &config_server.uri(), "tok-from-config")?;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .env("FNN_RPC_URL", env_server.uri())
        .env("FNN_AUTH_TOKEN", "tok-from-env")
        .arg("info")
        .assert()
        .success();

    Ok(())
}

#[tokio::test]
async fn cli_overrides_env_and_config() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;

    let config_server = MockServer::start().await;
    let env_server = MockServer::start().await;
    let cli_server = MockServer::start().await;

    let node_info_body_cli = make_node_info_response("33");
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .and(header("authorization", "Bearer tok-from-cli"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(node_info_body_cli, "application/json"),
        )
        .expect(1)
        .mount(&cli_server)
        .await;

    write_config(&tmp, &config_server.uri(), "tok-from-config")?;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .env("FNN_RPC_URL", env_server.uri())
        .env("FNN_AUTH_TOKEN", "tok-from-env")
        .arg("--url")
        .arg(cli_server.uri())
        .arg("--auth-token")
        .arg("tok-from-cli")
        .arg("info")
        .assert()
        .success();

    Ok(())
}

#[tokio::test]
async fn url_auto_prepends_http_when_missing_scheme() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    // Use URL without scheme (strip the leading http://)
    let binding = mock_server.uri();
    let raw = binding.trim_start_matches("http://");

    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(make_node_info_response("41"), "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--url")
        .arg(raw)
        .arg("info")
        .assert()
        .success();

    Ok(())
}

#[tokio::test]
async fn invalid_url_duplicate_scheme_fails_fast() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;

    // Provide a malformed URL with duplicate scheme
    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--url")
        .arg("http://http://example.com")
        .arg("info");

    let assert = cmd.assert();
    assert.failure().stderr(
        predicate::str::contains("duplicate scheme").or(predicate::str::contains("Invalid URL")),
    );

    Ok(())
}

#[tokio::test]
async fn http_error_status_is_reported() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    // Return a 500 with a body
    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .respond_with(ResponseTemplate::new(500).set_body_string("server oops"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    let assert = cmd
        .env("HOME", tmp.path())
        .arg("--url")
        .arg(mock_server.uri())
        .arg("info")
        .assert();

    assert
        .failure()
        .stderr(predicate::str::contains("HTTP error"));

    Ok(())
}

#[tokio::test]
async fn auth_token_file_trims_whitespace() -> Result<(), Box<dyn std::error::Error>> {
    let tmp = TempDir::new()?;
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(body_string_contains("\"method\":\"node_info\""))
        .and(header("authorization", "Bearer trimmed-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(make_node_info_response("51"), "application/json"),
        )
        .expect(1)
        .mount(&mock_server)
        .await;

    // write token file with surrounding whitespace/newline
    let token_file = tmp.path().join("token_ws.txt");
    fs::write(&token_file, "  trimmed-token\n")?;

    let mut cmd = assert_cmd::Command::cargo_bin("fnn-cli")?;
    cmd.env("HOME", tmp.path())
        .arg("--url")
        .arg(mock_server.uri())
        .arg("--auth-token-file")
        .arg(token_file)
        .arg("info")
        .assert()
        .success();

    Ok(())
}
