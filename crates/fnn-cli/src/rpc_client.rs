use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A lightweight JSON-RPC 2.0 client for communicating with FNN.
#[derive(Clone)]
pub struct RpcClient {
    url: String,
    client: reqwest::Client,
    raw_data: bool,
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
    pub fn new(url: &str, raw_data: bool) -> Self {
        // Auto-prepend http:// if no scheme is provided
        let url = if url.starts_with("http://") || url.starts_with("https://") {
            url.to_string()
        } else {
            format!("http://{}", url)
        };
        Self {
            url,
            client: reqwest::Client::new(),
            raw_data,
        }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn raw_data(&self) -> bool {
        self.raw_data
    }

    /// Sends a JSON-RPC request and returns the result.
    pub async fn call(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            params,
            id: 1,
        };

        let response = self
            .client
            .post(&self.url)
            .json(&request)
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

    /// Sends a JSON-RPC request with a single param object.
    pub async fn call_with_params(&self, method: &str, params: Value) -> Result<Value> {
        self.call(method, vec![params]).await
    }

    /// Sends a JSON-RPC request with no params.
    pub async fn call_no_params(&self, method: &str) -> Result<Value> {
        self.call(method, vec![]).await
    }
}
