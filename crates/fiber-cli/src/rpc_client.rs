use std::net::IpAddr;

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
        // Strip all whitespace/newlines from auth token (common when pasting or reading from file)
        let auth_token = auth_token
            .map(|t| t.chars().filter(|c| !c.is_whitespace()).collect::<String>())
            .filter(|t| !t.is_empty());
        let has_auth_token = auth_token.is_some();
        let has_explicit_scheme = url.starts_with("http://") || url.starts_with("https://");

        // Auto-prepend http:// if no scheme is provided
        let url = if has_explicit_scheme {
            url.to_string()
        } else {
            format!("http://{}", url)
        };
        // Validate the URL
        let parsed_url = Self::validate_url(&url)?;
        if has_auth_token {
            Self::validate_authenticated_url(&parsed_url, has_explicit_scheme)?;
        }

        Ok(Self {
            url,
            client: reqwest::Client::new(),
            raw_data,
            auth_token,
        })
    }

    /// Validate the URL, catching common mistakes like duplicate schemes.
    fn validate_url(url: &str) -> Result<reqwest::Url> {
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

        Ok(parsed)
    }

    fn validate_authenticated_url(url: &reqwest::Url, has_explicit_scheme: bool) -> Result<()> {
        if url.scheme() == "https" || Self::is_loopback_url(url) {
            return Ok(());
        }

        if !has_explicit_scheme {
            let mut https_url = url.clone();
            let _ = https_url.set_scheme("https");
            return Err(anyhow!(
                "Refusing to send RPC auth token over implicit plaintext HTTP to '{}'. Use '{}' or specify 'http://' explicitly if plaintext transport is intentional.",
                url,
                https_url
            ));
        }

        eprintln!(
            "warning: sending RPC auth token over plaintext HTTP to '{}'; use HTTPS unless this is intentional",
            url
        );
        Ok(())
    }

    fn is_loopback_url(url: &reqwest::Url) -> bool {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);

        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .map(|addr| addr.is_loopback())
                .unwrap_or(false)
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
mod tests;
