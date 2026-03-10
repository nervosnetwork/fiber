use anyhow::{anyhow, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

/// A lightweight JSON-RPC 2.0 client for communicating with FNN.
#[derive(Clone, Debug)]
pub struct RpcClient {
    url: String,
    client: reqwest::Client,
    raw_data: bool,
    auth_token: Option<String>,
}

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: String,
    params: Vec<Value>,
    id: u64,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Option<Value>,
    error: Option<JsonRpcError>,
    #[allow(dead_code)]
    id: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[allow(dead_code)]
    data: Option<Value>,
}

impl RpcClient {
    pub fn new(url: &str, raw_data: bool, auth_token: Option<String>) -> Result<Self> {
        // Auto-prepend http:// if no scheme is provided
        let url = if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("http://{}", url)
        };
        // Validate the URL
        Self::validate_url(&url)?;
        // Strip all whitespace/newlines from auth token (common when pasting or reading from file)
        let auth_token = auth_token
            .map(|t| t.chars().filter(|c| !c.is_whitespace()).collect::<String>())
            .filter(|t| !t.is_empty());
        Ok(Self {
            url,
            client: reqwest::Client::new(),
            raw_data,
            auth_token,
        })
    }

    /// Validate the URL, catching common mistakes like duplicate schemes.
    fn validate_url(url: &str) -> Result<()> {
        // Check for duplicate scheme (e.g. "http://http://..." or "http://https://...")
        let after_scheme = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"));
        if let Some(rest) = after_scheme {
            if rest.starts_with("http://") || rest.starts_with("https://") {
                return Err(anyhow!(
                    "Invalid URL '{}': duplicate scheme detected. Did you mean '{}'?",
                    url,
                    rest
                ));
            }
        }

        // Try to parse the URL to catch other malformed URLs
        let parsed =
            reqwest::Url::parse(url).map_err(|e| anyhow!("Invalid URL '{}': {}", url, e))?;

        // Ensure we have a host
        if parsed.host().is_none() {
            return Err(anyhow!(
                "Invalid URL '{}': missing host. Expected format: http://host:port",
                url
            ));
        }

        Ok(())
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn raw_data(&self) -> bool {
        self.raw_data
    }

    pub fn auth_token(&self) -> Option<&str> {
        self.auth_token.as_deref()
    }

    /// Sends a JSON-RPC request and returns the result.
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
            id: 1,
        };

        let mut req_builder = self.client.post(&self.url).json(&request);

        if let Some(token) = &self.auth_token {
            req_builder = req_builder.bearer_auth(token);
        }

        let response = req_builder
            .send()
            .await
            .map_err(|e| anyhow!("Failed to send request to {}: {}", self.url, e))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "HTTP error {}: {}",
                status,
                body.chars().take(200).collect::<String>()
            ));
        }

        let body: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| anyhow!("Failed to parse JSON-RPC response: {}", e))?;

        if let Some(error) = body.error {
            return Err(anyhow!(
                "RPC error (code {}): {}",
                error.code,
                error.message
            ));
        }

        Ok(body.result.unwrap_or(Value::Null))
    }

    /// Sends a typed JSON-RPC request with positional params and deserializes the result.
    pub async fn call_typed_with_values<R>(&self, method: &str, params: Vec<Value>) -> Result<R>
    where
        R: DeserializeOwned,
    {
        let value = self.call(method, params).await?;
        serde_json::from_value(value).map_err(|e| {
            anyhow!(
                "Failed to deserialize JSON-RPC result for {}: {}",
                method,
                e
            )
        })
    }

    /// Sends a typed JSON-RPC request with a single param object and deserializes the result.
    pub async fn call_typed<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let value = serde_json::to_value(params)
            .map_err(|e| anyhow!("Failed to serialize JSON-RPC params for {}: {}", method, e))?;
        self.call_typed_with_values(method, vec![value]).await
    }

    /// Sends a typed JSON-RPC request with no params and deserializes the result.
    pub async fn call_typed_no_params<R>(&self, method: &str) -> Result<R>
    where
        R: DeserializeOwned,
    {
        self.call_typed_with_values(method, vec![]).await
    }

    /// Sends a JSON-RPC request with no params.
    pub async fn call_no_params(&self, method: &str) -> Result<Value> {
        self.call(method, vec![]).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_auto_prepends_http() {
        let client = RpcClient::new("127.0.0.1:8227", false, None).unwrap();
        assert_eq!(client.url(), "http://127.0.0.1:8227");
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
}
