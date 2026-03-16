use regex::Regex;
use reqwest::{Client, Error as ReqwestError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::time::sleep;

const NODE_3_PUBKEY: &str = "03032b99943822e721a651c5a5b9621043017daa9dc3ec81d83215fd2e25121187";
const GOSSIP_UPDATE_FEE_BASE: u128 = 0x2710;
const GOSSIP_APPLIED_LATENCY_METRIC: &str = "fiber.gossip.propagation_applied_latency_ms";
const CHANNEL_UPDATE_MESSAGE_TYPE: &str = "channel_update";
const DEFAULT_NODE1_METRICS_URL: &str = "http://127.0.0.1:29114/metrics";
const DEFAULT_NODE2_METRICS_URL: &str = "http://127.0.0.1:29115/metrics";
const DEFAULT_NODE3_METRICS_URL: &str = "http://127.0.0.1:29116/metrics";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BenchmarkResult {
    pub timestamp: String,
    pub test_duration_secs: f64,
    pub worker_threads: usize,
    pub total_transactions: u64,
    pub successful_transactions: u64,
    pub failed_transactions: u64,
    pub success_rate_percent: f64,
    pub tps: f64,
    pub avg_latency_ms: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GossipBenchmarkResult {
    pub timestamp: String,
    pub test_duration_secs: f64,
    pub update_interval_ms: u64,
    #[serde(default)]
    pub load_mode: String,
    #[serde(default)]
    pub burst_updates_per_window: u64,
    #[serde(default)]
    pub burst_window_ms: u64,
    #[serde(default)]
    pub burst_windows: u64,
    #[serde(default)]
    pub burst_updates_sent: u64,
    #[serde(default)]
    pub peak_send_updates_per_sec: f64,
    pub total_updates: u64,
    #[serde(default)]
    pub workload_channels: Vec<GossipWorkloadChannelResult>,
    pub full_convergence_updates: u64,
    pub full_convergence_rate_percent: f64,
    pub avg_convergence_rate_percent: f64,
    pub latency_samples: u64,
    pub avg_latency_ms: f64,
    pub p50_latency_ms: f64,
    pub p90_latency_ms: f64,
    pub p99_latency_ms: f64,
    pub max_latency_ms: f64,
    #[serde(default)]
    pub latency_le_50ms_percent: f64,
    #[serde(default)]
    pub latency_le_100ms_percent: f64,
    #[serde(default)]
    pub latency_le_200ms_percent: f64,
    #[serde(default)]
    pub latency_le_500ms_percent: f64,
    pub updates_per_sec: f64,
    pub metrics_collected: bool,
    pub received_broadcast_messages: u64,
    pub applied_broadcast_messages: u64,
    pub duplicate_broadcast_messages: u64,
    pub rejected_broadcast_messages: u64,
    pub received_gossip_bytes: u64,
    pub sent_gossip_bytes: u64,
    pub duplicate_rate_percent: f64,
    #[serde(default)]
    pub rejected_rate_percent: f64,
    pub bytes_per_applied_message: f64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct GossipWorkloadChannelResult {
    pub source_node: String,
    pub peer_node: String,
    pub channel_id: String,
    pub channel_outpoint: String,
    pub updates: u64,
}

#[derive(Error, Debug)]
pub enum TestError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] ReqwestError),
    #[error("JSON parsing failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("RPC error: {0}")]
    Rpc(String),
    #[error("Assertion failed: {0}")]
    Assertion(String),
    #[error("Timeout")]
    Timeout,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Config parse error: {0}")]
    ConfigParse(String),
}

pub type TestResult<T> = Result<T, TestError>;

#[derive(Clone)]
pub struct NodeConfig {
    pub rpc_url: String,
    pub pubkey: String,
    pub addr: String,
    pub metrics_url: String,
}

pub struct TestContext {
    pub client: Client,
    pub node1: NodeConfig,
    pub node2: NodeConfig,
    pub node3: NodeConfig,
    pub ckb_rpc_url: String,
    pub udt_code_hash: String,
    pub lnd_bob_rpc_url: String,
    pub lnd_ingrid_rpc_url: String,
    pub n1n2_channel_id: Option<String>,
    pub n2n3_channel_id: Option<String>,
}

impl TestContext {
    pub fn new() -> Self {
        let client = Client::new();

        // Try to load configuration from Bruno file
        match Self::load_config_from_bruno() {
            Ok(config_vars) => Self::from_config_vars(client, config_vars),
            Err(_) => {
                eprintln!("Warning: No config file found, using environment variables or defaults");
                panic!("No config file found");
            }
        }
    }

    /// Create TestContext from Bruno environment file content
    pub fn from_bruno_content(content: &str) -> Result<Self, TestError> {
        let client = Client::new();
        let config_vars = Self::parse_bruno_content(content)?;
        Ok(Self::from_config_vars(client, config_vars))
    }

    /// Load configuration from default Bruno file locations
    fn load_config_from_bruno() -> Result<HashMap<String, String>, TestError> {
        let path = "../bruno/environments/test.bru";

        if Path::new(path).exists() {
            let content = fs::read_to_string(path)?;
            return Self::parse_bruno_content(&content);
        }

        Err(TestError::ConfigParse("No config file found".to_string()))
    }

    /// Parse Bruno environment file content into a HashMap
    fn parse_bruno_content(content: &str) -> Result<HashMap<String, String>, TestError> {
        let mut vars = HashMap::new();

        // Regex to match Bruno variable definitions: KEY: value
        let var_regex = Regex::new(r"^\s*([A-Z_][A-Z0-9_]*)\s*:\s*(.+)$")
            .map_err(|e| TestError::ConfigParse(format!("Regex compilation failed: {}", e)))?;

        // Parse each line
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty()
                || line.starts_with("vars")
                || line.starts_with("{")
                || line.starts_with("}")
            {
                continue;
            }

            if let Some(captures) = var_regex.captures(line) {
                let key = captures.get(1).unwrap().as_str();
                let value = captures.get(2).unwrap().as_str().trim();
                vars.insert(key.to_string(), value.to_string());
            }
        }

        Ok(vars)
    }

    /// Helper function to get configuration value with priority: env var > config file > panic
    fn get_config_value(vars: &HashMap<String, String>, key: &str) -> String {
        std::env::var(key).unwrap_or_else(|_| vars.get(key).cloned().unwrap())
    }

    /// Helper function to get optional configuration value with priority: env var > config file > default
    fn get_config_value_with_default(
        vars: &HashMap<String, String>,
        key: &str,
        default: &str,
    ) -> String {
        std::env::var(key)
            .ok()
            .or_else(|| vars.get(key).cloned())
            .unwrap_or_else(|| default.to_string())
    }

    /// Create TestContext from parsed configuration variables
    fn from_config_vars(client: Client, vars: HashMap<String, String>) -> Self {
        Self {
            client,
            node1: NodeConfig {
                rpc_url: Self::get_config_value(&vars, "NODE1_RPC_URL"),
                pubkey: Self::get_config_value(&vars, "NODE1_PUBKEY"),
                addr: Self::get_config_value(&vars, "NODE1_ADDR"),
                metrics_url: Self::get_config_value_with_default(
                    &vars,
                    "NODE1_METRICS_URL",
                    DEFAULT_NODE1_METRICS_URL,
                ),
            },
            node2: NodeConfig {
                rpc_url: Self::get_config_value(&vars, "NODE2_RPC_URL"),
                pubkey: Self::get_config_value(&vars, "NODE2_PUBKEY"),
                addr: Self::get_config_value(&vars, "NODE2_ADDR"),
                metrics_url: Self::get_config_value_with_default(
                    &vars,
                    "NODE2_METRICS_URL",
                    DEFAULT_NODE2_METRICS_URL,
                ),
            },
            node3: NodeConfig {
                rpc_url: Self::get_config_value(&vars, "NODE3_RPC_URL"),
                pubkey: Self::get_config_value(&vars, "NODE3_PUBKEY"),
                addr: Self::get_config_value(&vars, "NODE3_ADDR"),
                metrics_url: Self::get_config_value_with_default(
                    &vars,
                    "NODE3_METRICS_URL",
                    DEFAULT_NODE3_METRICS_URL,
                ),
            },
            ckb_rpc_url: Self::get_config_value(&vars, "CKB_RPC_URL"),
            udt_code_hash: Self::get_config_value(&vars, "UDT_CODE_HASH"),
            lnd_bob_rpc_url: Self::get_config_value(&vars, "LND_BOB_RPC_URL"),
            lnd_ingrid_rpc_url: Self::get_config_value(&vars, "LND_INGRID_RPC_URL"),
            n1n2_channel_id: None,
            n2n3_channel_id: None,
        }
    }

    pub async fn fetch_metrics_text(&self, metrics_url: &str) -> TestResult<String> {
        let response = self.client.get(metrics_url).send().await?;
        let body = response.text().await?;
        Ok(body)
    }

    /// Send a JSON-RPC request to the specified URL
    pub async fn send_rpc_request(
        &self,
        url: &str,
        method: &str,
        params: Value,
    ) -> TestResult<Value> {
        let payload = json!({
            "id": "42",
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        //eprintln!("Sending request to {}: {}", url, payload);

        let response = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        //eprintln!("Response Status: {:?}", response);
        let result: Value = response.json().await?;

        // Check for RPC errors
        if let Some(error) = result.get("error") {
            return Err(TestError::Rpc(error.to_string()));
        }

        Ok(result)
    }

    /// Connect one node to another
    pub async fn connect_peer(&self, from_url: &str, target_addr: &str) -> TestResult<()> {
        let params = json!([{"address": target_addr}]);
        let result = self
            .send_rpc_request(from_url, "connect_peer", params)
            .await?;

        // Assert no error and result is null
        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in connect_peer".to_string(),
            ));
        }

        if !result.get("result").unwrap().is_null() {
            return Err(TestError::Assertion(
                "Expected null result in connect_peer".to_string(),
            ));
        }

        // Wait for connection to establish
        sleep(Duration::from_millis(1000)).await;

        Ok(())
    }

    /// Open a channel between two nodes
    pub async fn open_channel(
        &self,
        from_url: &str,
        pubkey: &str,
        funding_amount: &str,
    ) -> TestResult<String> {
        let params = json!([{
            "pubkey": pubkey,
            "funding_amount": funding_amount
        }]);

        let result = self
            .send_rpc_request(from_url, "open_channel", params)
            .await?;

        // Assert no error and temporary_channel_id is defined
        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in open_channel".to_string(),
            ));
        }

        let temp_channel_id = result["result"]["temporary_channel_id"]
            .as_str()
            .ok_or_else(|| TestError::Assertion("Expected temporary_channel_id".to_string()))?;

        sleep(Duration::from_millis(2000)).await;

        Ok(temp_channel_id.to_string())
    }

    /// List channels for a specific peer
    pub async fn list_channels(&self, node_url: &str, pubkey: Option<&str>) -> TestResult<Value> {
        let params = if let Some(pubkey) = pubkey {
            json!([{"pubkey": pubkey}])
        } else {
            json!([{}])
        };

        let result = self
            .send_rpc_request(node_url, "list_channels", params)
            .await?;

        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in list_channels".to_string(),
            ));
        }

        if result["result"]["channels"].as_array().is_none() {
            return Err(TestError::Assertion("Expected channels array".to_string()));
        }

        sleep(Duration::from_millis(2000)).await;

        Ok(result)
    }

    /// Get auto-accepted channel ID from a node
    pub async fn get_auto_accepted_channel(
        &self,
        node_url: &str,
        pubkey: &str,
    ) -> TestResult<String> {
        let result = self.list_channels(node_url, Some(pubkey)).await?;

        let channels = result["result"]["channels"]
            .as_array()
            .ok_or_else(|| TestError::Assertion("Expected channels array".to_string()))?;

        if channels.is_empty() {
            return Err(TestError::Assertion(
                "Expected at least one channel".to_string(),
            ));
        }

        let channel = &channels[0];
        let channel_id = channel["channel_id"]
            .as_str()
            .ok_or_else(|| TestError::Assertion("Expected channel_id".to_string()))?;

        // Validate channel properties
        if channel.get("enabled").is_none() {
            return Err(TestError::Assertion("Expected enabled field".to_string()));
        }

        if channel.get("tlc_expiry_delta").is_none() {
            return Err(TestError::Assertion(
                "Expected tlc_expiry_delta field".to_string(),
            ));
        }

        if channel.get("tlc_fee_proportional_millionths").is_none() {
            return Err(TestError::Assertion(
                "Expected tlc_fee_proportional_millionths field".to_string(),
            ));
        }

        //println!("Channel info: {}", serde_json::to_string_pretty(channel)?);

        Ok(channel_id.to_string())
    }

    /// Get a public channel ID and outpoint from a node for a peer.
    pub async fn get_public_channel(
        &self,
        node_url: &str,
        peer_id: &str,
    ) -> TestResult<(String, String)> {
        let result = self.list_channels(node_url, Some(peer_id)).await?;
        let channels = result["result"]["channels"]
            .as_array()
            .ok_or_else(|| TestError::Assertion("Expected channels array".to_string()))?;

        for channel in channels {
            let is_public = channel["is_public"].as_bool().unwrap_or(false);
            if !is_public {
                continue;
            }

            let channel_id = channel["channel_id"]
                .as_str()
                .ok_or_else(|| TestError::Assertion("Expected channel_id".to_string()))?;
            let outpoint = channel["channel_outpoint"]
                .as_str()
                .ok_or_else(|| TestError::Assertion("Expected channel_outpoint".to_string()))?;
            return Ok((channel_id.to_string(), outpoint.to_string()));
        }

        Err(TestError::Assertion(format!(
            "No public channel found for peer {}",
            peer_id
        )))
    }

    /// Update channel fee to trigger gossip channel update broadcast.
    pub async fn update_channel_fee(
        &self,
        node_url: &str,
        channel_id: &str,
        fee_rate: u128,
    ) -> TestResult<()> {
        let params = json!([{
            "channel_id": channel_id,
            "tlc_fee_proportional_millionths": format!("0x{:x}", fee_rate)
        }]);
        let result = self
            .send_rpc_request(node_url, "update_channel", params)
            .await?;

        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in update_channel".to_string(),
            ));
        }
        if !result["result"].is_null() {
            return Err(TestError::Assertion(
                "Expected null result in update_channel".to_string(),
            ));
        }
        Ok(())
    }

    /// Generate blocks/epochs on CKB
    pub async fn generate_ckb_epochs(&self, epochs: &str) -> TestResult<()> {
        let params = json!([epochs]);

        let result = self
            .send_rpc_request(&self.ckb_rpc_url, "generate_epochs", params)
            .await?;

        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in generate_epochs".to_string(),
            ));
        }

        sleep(Duration::from_millis(5000)).await;

        Ok(())
    }

    /// Validate channel balance
    pub async fn validate_channel_balance(
        &self,
        node_url: &str,
        pubkey: &str,
        expected_remote: &str,
        expected_local: &str,
    ) -> TestResult<()> {
        let result = self.list_channels(node_url, Some(pubkey)).await?;

        let channels = result["result"]["channels"]
            .as_array()
            .ok_or_else(|| TestError::Assertion("Expected channels array".to_string()))?;

        if channels.is_empty() {
            return Err(TestError::Assertion(
                "Expected at least one channel".to_string(),
            ));
        }

        let channel = &channels[0];
        let remote_balance = channel["remote_balance"]
            .as_str()
            .ok_or_else(|| TestError::Assertion("Expected remote_balance".to_string()))?;
        let local_balance = channel["local_balance"]
            .as_str()
            .ok_or_else(|| TestError::Assertion("Expected local_balance".to_string()))?;

        if remote_balance != expected_remote {
            return Err(TestError::Assertion(format!(
                "Remote balance mismatch: expected {}, got {}",
                expected_remote, remote_balance
            )));
        }

        if local_balance != expected_local {
            return Err(TestError::Assertion(format!(
                "Local balance mismatch: expected {}, got {}",
                expected_local, local_balance
            )));
        }

        println!(
            "Channel balance validation passed: remote={}, local={}",
            remote_balance, local_balance
        );

        Ok(())
    }

    /// Send a payment (keysend mode)
    pub async fn send_payment(
        &self,
        from_url: &str,
        target_pubkey: &str,
        amount: &str,
        keysend: bool,
    ) -> TestResult<Value> {
        let params = json!([{
            "target_pubkey": target_pubkey,
            "amount": amount,
            "keysend": keysend
        }]);

        sleep(Duration::from_millis(1000)).await;

        let result = self
            .send_rpc_request(from_url, "send_payment", params)
            .await?;

        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in send_payment".to_string(),
            ));
        }

        Ok(result)
    }

    pub async fn get_payment_status(
        &self,
        from_url: &str,
        payment_hash: &str,
    ) -> TestResult<String> {
        let params = json!([{"payment_hash": payment_hash}]);

        let result = self
            .send_rpc_request(from_url, "get_payment", params)
            .await?;

        if result.get("error").is_some() {
            return Err(TestError::Assertion(
                "Expected no error in get_payment".to_string(),
            ));
        }

        //eprintln!("get_payment_result: {:}", result);
        let status = result["result"]["status"]
            .as_str()
            .ok_or_else(|| TestError::Assertion("Expected status field".to_string()))?;

        Ok(status.to_string())
    }

    pub async fn is_payment_success(&self, from_url: &str, payment_hash: &str) -> TestResult<bool> {
        let status = self.get_payment_status(from_url, payment_hash).await?;
        Ok(status == "Success")
    }

    pub async fn wait_until_final_status(
        &self,
        from_url: &str,
        payment_hash: &str,
    ) -> TestResult<String> {
        let timeout = Duration::from_secs(60);
        let start = tokio::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(TestError::Timeout);
            }

            let status = self.get_payment_status(from_url, payment_hash).await?;
            if status == "Success" || status == "Failed" {
                return Ok(status);
            }

            sleep(Duration::from_millis(5)).await;
        }
    }
}

/// Full integration test sequence
pub async fn run_integration_test() -> TestResult<()> {
    let mut ctx = TestContext::new();

    println!("📡 Step 1: Connecting peers...");
    ctx.connect_peer(&ctx.node2.rpc_url, &ctx.node1.addr)
        .await?;
    ctx.connect_peer(&ctx.node3.rpc_url, &ctx.node2.addr)
        .await?;

    println!("📡 Step 2: Opening channels...");
    let _temp_channel_1 = ctx
        .open_channel(&ctx.node1.rpc_url, &ctx.node2.pubkey, "0x377aab54d000")
        .await?;
    ctx.n1n2_channel_id = Some(
        ctx.get_auto_accepted_channel(&ctx.node2.rpc_url, &ctx.node1.pubkey)
            .await?,
    );

    println!("📡 Step 3: Generating blocks for channel 1...");
    ctx.generate_ckb_epochs("0x2").await?;

    let _temp_channel_2 = ctx
        .open_channel(&ctx.node2.rpc_url, &ctx.node3.pubkey, "0x377aab54d000")
        .await?;
    ctx.n2n3_channel_id = Some(
        ctx.get_auto_accepted_channel(&ctx.node3.rpc_url, &ctx.node2.pubkey)
            .await?,
    );

    println!("📡 Step 4: Generating blocks for channel 2...");
    ctx.generate_ckb_epochs("0x2").await?;

    println!("✅ Step 5: Validating channel balances...");
    ctx.validate_channel_balance(
        &ctx.node2.rpc_url,
        &ctx.node3.pubkey,
        "0x0",
        "0x37785d3ecd00",
    )
    .await?;

    println!("💰 Step 6: Sending keysend payment...");
    ctx.send_payment(&ctx.node1.rpc_url, NODE_3_PUBKEY, "0x1F4", true)
        .await?;

    println!("🎉 Integration test completed successfully!");
    Ok(())
}

/// Run benchmark test with specified duration and number of workers using native threads
pub fn run_benchmark_test(duration: Duration, worker_number: usize) -> TestResult<BenchmarkResult> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    let ctx = Arc::new(TestContext::new());
    let success_count = Arc::new(AtomicU64::new(0));
    let error_count = Arc::new(AtomicU64::new(0));
    let total_count = Arc::new(AtomicU64::new(0));

    println!(
        "🚀 Native thread benchmark test: {} worker threads, duration {:?}",
        worker_number, duration
    );

    let start_time = Instant::now();
    let end_time = start_time + duration;
    let mut thread_handles = Vec::new();

    // Start worker threads
    for worker_id in 0..worker_number {
        let ctx_clone = ctx.clone();
        let success_count_clone = success_count.clone();
        let error_count_clone = error_count.clone();
        let total_count_clone = total_count.clone();

        let handle = thread::spawn(move || {
            // Create independent single-threaded tokio runtime for each thread
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            // println!(
            //     "🚀 Worker thread {} started, thread ID: {:?}",
            //     worker_id,
            //     thread::current().id()
            // );

            rt.block_on(async move {
                let mut local_success = 0u64;
                let mut local_error = 0u64;
                let mut local_total = 0u64;

                while Instant::now() < end_time {
                    let Ok(result) = ctx_clone
                        .send_payment(
                            &ctx_clone.node1.rpc_url,
                            NODE_3_PUBKEY,
                            "0x64", // 100 units
                            true,
                        )
                        .await
                    else {
                        continue;
                    };

                    let res = ctx_clone
                        .wait_until_final_status(
                            &ctx_clone.node1.rpc_url,
                            result["result"]["payment_hash"].as_str().unwrap(),
                        )
                        .await;

                    match res {
                        Ok(status) if status == "Success" => {
                            local_success += 1;
                            success_count_clone.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(status) if status == "Failed" => {
                            local_error += 1;
                            error_count_clone.fetch_add(1, Ordering::Relaxed);
                            if local_error % 10 == 0 {
                                eprintln!("Worker {}: Error #{}: ", worker_id, local_error);
                            }
                        }
                        _ => {}
                    }
                    local_total += 1;
                    total_count_clone.fetch_add(1, Ordering::Relaxed);
                }

                println!(
                    "🏁 Worker {} completed: {} success, {} errors, {} total",
                    worker_id, local_success, local_error, local_total
                );
            });
        });

        thread_handles.push(handle);
    }

    // Start progress monitoring thread
    let progress_handle = {
        let success_count_clone = success_count.clone();
        let error_count_clone = error_count.clone();
        let total_count_clone = total_count.clone();

        thread::spawn(move || {
            while start_time.elapsed() < duration {
                thread::sleep(std::time::Duration::from_secs(10));

                let elapsed = start_time.elapsed().as_secs_f64();
                let current_success = success_count_clone.load(Ordering::Relaxed);
                let current_error = error_count_clone.load(Ordering::Relaxed);
                let current_total = total_count_clone.load(Ordering::Relaxed);
                let current_tps = current_success as f64 / elapsed;

                println!(
                    "📊 Progress report: {:.1}s elapsed, {} success, {} errors, {} total, TPS: {:.2}",
                    elapsed, current_success, current_error, current_total, current_tps
                );
            }
        })
    };

    // Wait for all worker threads to complete
    for (i, handle) in thread_handles.into_iter().enumerate() {
        if let Err(e) = handle.join() {
            eprintln!("Worker thread {} execution error: {:?}", i, e);
        }
    }

    // Wait for progress monitoring thread
    let _ = progress_handle.join();

    // Calculate final statistics
    let final_elapsed = start_time.elapsed();
    let final_success = success_count.load(Ordering::Relaxed);
    let final_error = error_count.load(Ordering::Relaxed);
    let final_total = total_count.load(Ordering::Relaxed);
    let final_tps = final_success as f64 / final_elapsed.as_secs_f64();
    let success_rate = if final_total > 0 {
        (final_success as f64 / final_total as f64) * 100.0
    } else {
        0.0
    };

    println!("\n🎯 Native thread benchmark test results:");
    println!("=====================================");
    println!("Test duration: {:.2} seconds", final_elapsed.as_secs_f64());
    println!("Worker threads: {}", worker_number);
    println!("Total transactions: {}", final_total);
    println!("Successful transactions: {}", final_success);
    println!("Failed transactions: {}", final_error);
    println!("Success rate: {:.2}%", success_rate);
    println!(
        "Average per-thread latency: {:.2} ms",
        (final_elapsed.as_millis() as f64) / (worker_number as f64)
    );
    println!("TPS (Transactions per second): {:.2}", final_tps);
    println!("=====================================\n");

    let benchmark_result = BenchmarkResult {
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string(),
        test_duration_secs: final_elapsed.as_secs_f64(),
        worker_threads: worker_number,
        total_transactions: final_total,
        successful_transactions: final_success,
        failed_transactions: final_error,
        success_rate_percent: success_rate,
        tps: final_tps,
        avg_latency_ms: (final_elapsed.as_millis() as f64) / (worker_number as f64),
    };

    Ok(benchmark_result)
}

#[derive(Debug, Clone, Copy)]
struct PrometheusSample<'a> {
    sample_name: &'a str,
    labels: Option<&'a str>,
    value: f64,
}

fn parse_prometheus_sample(line: &str) -> Option<PrometheusSample<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let mut parts = line.split_whitespace();
    let sample_with_labels = parts.next()?;
    let value_text = parts.next()?;
    let value = value_text.parse::<f64>().ok()?;

    let (sample_name, labels) = if let Some((name, labels_with_suffix)) =
        sample_with_labels.split_once('{')
    {
        let labels = labels_with_suffix.strip_suffix('}')?;
        (name, Some(labels))
    } else {
        (sample_with_labels, None)
    };

    Some(PrometheusSample {
        sample_name,
        labels,
        value,
    })
}

fn parse_prometheus_label_value<'a>(labels: Option<&'a str>, key: &str) -> Option<&'a str> {
    let labels = labels?;
    for entry in labels.split(',') {
        let (name, value) = entry.split_once('=')?;
        if name.trim() != key {
            continue;
        }
        let value = value.trim();
        return Some(
            value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value),
        );
    }
    None
}

fn labels_match(labels: Option<&str>, required: &[(&str, &str)]) -> bool {
    required
        .iter()
        .all(|(key, expected)| parse_prometheus_label_value(labels, key) == Some(*expected))
}

fn parse_prometheus_bucket_le(labels: Option<&str>) -> Option<f64> {
    let le = parse_prometheus_label_value(labels, "le")?;
    match le {
        "+Inf" | "Inf" | "inf" => Some(f64::INFINITY),
        _ => le.parse::<f64>().ok(),
    }
}

fn metric_name_matches(sample_name: &str, metric_name: &str, metric_name_sanitized: &str) -> bool {
    sample_name == metric_name || sample_name == metric_name_sanitized
}

fn f64_to_u64_saturating(value: f64) -> u64 {
    if !value.is_finite() || value.is_sign_negative() {
        0
    } else if value >= u64::MAX as f64 {
        u64::MAX
    } else {
        value as u64
    }
}

#[derive(Debug, Clone, Default)]
struct HistogramSnapshot {
    buckets: Vec<(f64, u64)>,
    sum: f64,
    count: u64,
}

impl HistogramSnapshot {
    fn add_assign(&mut self, other: &Self) {
        self.sum += other.sum.max(0.0);
        self.count = self.count.saturating_add(other.count);
        for (le, value) in &other.buckets {
            if let Some((_, current)) = self.buckets.iter_mut().find(|(left, _)| *left == *le) {
                *current = current.saturating_add(*value);
            } else {
                self.buckets.push((*le, *value));
            }
        }
        self.buckets.sort_by(|(left, _), (right, _)| left.total_cmp(right));
    }

    fn saturating_sub(&self, baseline: &Self) -> Self {
        let mut buckets = Vec::with_capacity(self.buckets.len());
        for (le, value) in &self.buckets {
            let baseline_value = baseline
                .buckets
                .iter()
                .find(|(baseline_le, _)| *baseline_le == *le)
                .map(|(_, bucket_value)| *bucket_value)
                .unwrap_or(0);
            buckets.push((*le, value.saturating_sub(baseline_value)));
        }
        buckets.sort_by(|(left, _), (right, _)| left.total_cmp(right));

        Self {
            buckets,
            sum: (self.sum - baseline.sum).max(0.0),
            count: self.count.saturating_sub(baseline.count),
        }
    }

    fn percentile(&self, percentile: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }

        let percentile = percentile.clamp(0.0, 1.0);
        let target_rank = ((self.count as f64) * percentile).ceil().max(1.0);
        let mut previous_bucket_count = 0u64;
        let mut previous_le = 0.0f64;
        let mut largest_finite_le = 0.0f64;

        for (le, cumulative_bucket_count) in &self.buckets {
            let cumulative_bucket_count = (*cumulative_bucket_count).max(previous_bucket_count);
            if cumulative_bucket_count > previous_bucket_count
                && (cumulative_bucket_count as f64) >= target_rank
            {
                if !le.is_finite() {
                    return largest_finite_le;
                }

                let bucket_count = cumulative_bucket_count - previous_bucket_count;
                if bucket_count == 0 {
                    return *le;
                }

                let rank_in_bucket =
                    ((target_rank - previous_bucket_count as f64) / bucket_count as f64)
                        .clamp(0.0, 1.0);
                return previous_le + (*le - previous_le) * rank_in_bucket;
            }

            previous_bucket_count = cumulative_bucket_count;
            if le.is_finite() {
                previous_le = *le;
                largest_finite_le = *le;
            }
        }

        largest_finite_le
    }

    fn average(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }

    fn max_latency_bound(&self) -> f64 {
        let mut previous_bucket_count = 0u64;
        let mut largest_finite_le = 0.0f64;
        for (le, cumulative_bucket_count) in &self.buckets {
            let cumulative_bucket_count = (*cumulative_bucket_count).max(previous_bucket_count);
            if cumulative_bucket_count > previous_bucket_count && le.is_finite() {
                largest_finite_le = *le;
            }
            previous_bucket_count = cumulative_bucket_count;
        }
        largest_finite_le
    }

    fn cumulative_count_le(&self, upper_bound: f64) -> u64 {
        let mut previous_bucket_count = 0u64;
        for (le, cumulative_bucket_count) in &self.buckets {
            let cumulative_bucket_count = (*cumulative_bucket_count).max(previous_bucket_count);
            if *le >= upper_bound {
                return cumulative_bucket_count;
            }
            previous_bucket_count = cumulative_bucket_count;
        }
        self.count
    }

    fn coverage_percent_le(&self, upper_bound: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.cumulative_count_le(upper_bound) as f64 * 100.0 / self.count as f64
    }
}

fn parse_prometheus_histogram(
    text: &str,
    metric_name: &str,
    required_labels: &[(&str, &str)],
) -> HistogramSnapshot {
    let sanitized_metric_name = metric_name.replace('.', "_");
    let bucket_name = format!("{metric_name}_bucket");
    let bucket_name_sanitized = format!("{sanitized_metric_name}_bucket");
    let count_name = format!("{metric_name}_count");
    let count_name_sanitized = format!("{sanitized_metric_name}_count");
    let sum_name = format!("{metric_name}_sum");
    let sum_name_sanitized = format!("{sanitized_metric_name}_sum");

    let mut snapshot = HistogramSnapshot::default();

    for line in text.lines() {
        let Some(sample) = parse_prometheus_sample(line) else {
            continue;
        };

        if metric_name_matches(&sample.sample_name, &bucket_name, &bucket_name_sanitized) {
            if !labels_match(sample.labels, required_labels) {
                continue;
            }
            let Some(le) = parse_prometheus_bucket_le(sample.labels) else {
                continue;
            };
            let value = f64_to_u64_saturating(sample.value);
            if let Some((_, current)) = snapshot.buckets.iter_mut().find(|(left, _)| *left == le) {
                *current = (*current).max(value);
            } else {
                snapshot.buckets.push((le, value));
            }
            continue;
        }

        if metric_name_matches(&sample.sample_name, &count_name, &count_name_sanitized) {
            if !labels_match(sample.labels, required_labels) {
                continue;
            }
            snapshot.count = snapshot.count.max(f64_to_u64_saturating(sample.value));
            continue;
        }

        if metric_name_matches(&sample.sample_name, &sum_name, &sum_name_sanitized)
            && labels_match(sample.labels, required_labels)
        {
            snapshot.sum = snapshot.sum.max(sample.value.max(0.0));
        }
    }

    snapshot
}

#[derive(Debug, Clone, Default)]
struct GossipMetricsSnapshot {
    received_broadcast_messages: u64,
    applied_broadcast_messages: u64,
    duplicate_broadcast_messages: u64,
    rejected_broadcast_messages: u64,
    received_gossip_bytes: u64,
    sent_gossip_bytes: u64,
    applied_channel_update_latency: HistogramSnapshot,
}

impl GossipMetricsSnapshot {
    fn add_assign(&mut self, other: &Self) {
        self.received_broadcast_messages = self
            .received_broadcast_messages
            .saturating_add(other.received_broadcast_messages);
        self.applied_broadcast_messages = self
            .applied_broadcast_messages
            .saturating_add(other.applied_broadcast_messages);
        self.duplicate_broadcast_messages = self
            .duplicate_broadcast_messages
            .saturating_add(other.duplicate_broadcast_messages);
        self.rejected_broadcast_messages = self
            .rejected_broadcast_messages
            .saturating_add(other.rejected_broadcast_messages);
        self.received_gossip_bytes = self
            .received_gossip_bytes
            .saturating_add(other.received_gossip_bytes);
        self.sent_gossip_bytes = self.sent_gossip_bytes.saturating_add(other.sent_gossip_bytes);
        self.applied_channel_update_latency
            .add_assign(&other.applied_channel_update_latency);
    }

    fn saturating_sub(self, baseline: Self) -> Self {
        Self {
            received_broadcast_messages: self
                .received_broadcast_messages
                .saturating_sub(baseline.received_broadcast_messages),
            applied_broadcast_messages: self
                .applied_broadcast_messages
                .saturating_sub(baseline.applied_broadcast_messages),
            duplicate_broadcast_messages: self
                .duplicate_broadcast_messages
                .saturating_sub(baseline.duplicate_broadcast_messages),
            rejected_broadcast_messages: self
                .rejected_broadcast_messages
                .saturating_sub(baseline.rejected_broadcast_messages),
            received_gossip_bytes: self
                .received_gossip_bytes
                .saturating_sub(baseline.received_gossip_bytes),
            sent_gossip_bytes: self.sent_gossip_bytes.saturating_sub(baseline.sent_gossip_bytes),
            applied_channel_update_latency: self
                .applied_channel_update_latency
                .saturating_sub(&baseline.applied_channel_update_latency),
        }
    }

    fn from_prometheus_text(text: &str) -> Self {
        Self {
            received_broadcast_messages: parse_prometheus_metric_sum(
                text,
                "fiber.gossip.received_broadcast_messages_total",
            ),
            applied_broadcast_messages: parse_prometheus_metric_sum(
                text,
                "fiber.gossip.applied_broadcast_messages_total",
            ),
            duplicate_broadcast_messages: parse_prometheus_metric_sum(
                text,
                "fiber.gossip.duplicate_broadcast_messages_total",
            ),
            rejected_broadcast_messages: parse_prometheus_metric_sum(
                text,
                "fiber.gossip.rejected_broadcast_messages_total",
            ),
            received_gossip_bytes: parse_prometheus_metric_sum(
                text,
                "fiber.gossip.received_bytes_total",
            ),
            sent_gossip_bytes: parse_prometheus_metric_sum(text, "fiber.gossip.sent_bytes_total"),
            applied_channel_update_latency: parse_prometheus_histogram(
                text,
                GOSSIP_APPLIED_LATENCY_METRIC,
                &[("message_type", CHANNEL_UPDATE_MESSAGE_TYPE)],
            ),
        }
    }
}

fn parse_prometheus_metric_sum(text: &str, metric_name: &str) -> u64 {
    let sanitized_name = metric_name.replace('.', "_");
    let mut total = 0f64;

    for line in text.lines() {
        let Some(sample) = parse_prometheus_sample(line) else {
            continue;
        };
        if metric_name_matches(sample.sample_name, metric_name, &sanitized_name) {
            total += sample.value.max(0.0);
        }
    }

    f64_to_u64_saturating(total)
}

async fn collect_gossip_metrics_per_node(
    ctx: &TestContext,
    metrics_urls: &[String],
) -> Option<Vec<GossipMetricsSnapshot>> {
    let mut snapshots = Vec::with_capacity(metrics_urls.len());
    for metrics_url in metrics_urls {
        let metrics_text = match ctx.fetch_metrics_text(metrics_url).await {
            Ok(text) => text,
            Err(error) => {
                eprintln!(
                    "Warning: failed to fetch metrics from {}: {}",
                    metrics_url, error
                );
                return None;
            }
        };
        snapshots.push(GossipMetricsSnapshot::from_prometheus_text(&metrics_text));
    }
    Some(snapshots)
}

fn sum_gossip_metrics(snapshots: &[GossipMetricsSnapshot]) -> GossipMetricsSnapshot {
    let mut total = GossipMetricsSnapshot::default();
    for snapshot in snapshots {
        total.add_assign(snapshot);
    }
    total
}

fn now_timestamp_millis_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn should_wait_for_next_channel_update_millis(last_sent_ms: Option<u64>, now_ms: u64) -> bool {
    matches!(last_sent_ms, Some(last) if now_ms <= last)
}

async fn wait_for_monotonic_channel_update_timestamp(
    channel_id: &str,
    last_update_ms_by_channel: &mut HashMap<String, u64>,
) {
    // Gossip currently keys channel updates by `(channel_outpoint, timestamp)` cursor.
    // If we emit multiple updates for the same channel within one millisecond, they can
    // share the same cursor and be treated as conflicting/rejected updates.
    // So in burst mode we enforce per-channel millisecond monotonicity here.
    let last_sent_ms = last_update_ms_by_channel.get(channel_id).copied();
    while should_wait_for_next_channel_update_millis(last_sent_ms, now_timestamp_millis_u64()) {
        sleep(Duration::from_millis(1)).await;
    }
    last_update_ms_by_channel.insert(channel_id.to_string(), now_timestamp_millis_u64());
}

async fn build_gossip_workload_channels(
    ctx: &TestContext,
) -> TestResult<Vec<GossipWorkloadChannelResult>> {
    let mut channels = Vec::new();

    for (peer_node, peer_pubkey) in [("node1", &ctx.node1.pubkey), ("node3", &ctx.node3.pubkey)] {
        match ctx.get_public_channel(&ctx.node2.rpc_url, peer_pubkey).await {
            Ok((channel_id, channel_outpoint)) => channels.push(GossipWorkloadChannelResult {
                source_node: "node2".to_string(),
                peer_node: peer_node.to_string(),
                channel_id,
                channel_outpoint,
                updates: 0,
            }),
            Err(error) => eprintln!(
                "Warning: skip gossip workload channel node2->{}: {}",
                peer_node, error
            ),
        }
    }

    let mut seen_channel_ids = HashSet::new();
    channels.retain(|channel| seen_channel_ids.insert(channel.channel_id.clone()));

    if channels.is_empty() {
        return Err(TestError::Assertion(
            "No public channels available for gossip workload from node2 to node1/node3".to_string(),
        ));
    }

    Ok(channels)
}

async fn trigger_next_gossip_update(
    ctx: &TestContext,
    workload_channels: &mut [GossipWorkloadChannelResult],
    next_fee_by_channel: &mut HashMap<String, u128>,
    last_update_ms_by_channel: &mut HashMap<String, u64>,
    workload_channel_index: &mut usize,
) -> TestResult<()> {
    let channel_index = *workload_channel_index % workload_channels.len();
    let channel_id = workload_channels[channel_index].channel_id.clone();
    wait_for_monotonic_channel_update_timestamp(&channel_id, last_update_ms_by_channel).await;
    let next_fee = {
        let fee = next_fee_by_channel.get_mut(&channel_id).ok_or_else(|| {
            TestError::Assertion(format!("Missing fee tracker for channel {}", channel_id))
        })?;
        *fee = fee.saturating_add(1);
        *fee
    };

    ctx.update_channel_fee(&ctx.node2.rpc_url, &channel_id, next_fee)
        .await?;
    workload_channels[channel_index].updates =
        workload_channels[channel_index].updates.saturating_add(1);
    *workload_channel_index = (*workload_channel_index).saturating_add(1);
    Ok(())
}

pub async fn run_gossip_benchmark_test(
    duration: Duration,
    update_interval: Duration,
    burst_updates_per_window: u64,
    burst_window: Duration,
) -> TestResult<GossipBenchmarkResult> {
    let ctx = TestContext::new();
    let burst_window = if burst_window.is_zero() {
        Duration::from_millis(1)
    } else {
        burst_window
    };
    let load_mode = if burst_updates_per_window > 0 {
        "burst"
    } else {
        "steady"
    };

    let mut workload_channels = build_gossip_workload_channels(&ctx).await?;
    let observer_metrics_urls = vec![ctx.node1.metrics_url.clone(), ctx.node3.metrics_url.clone()];
    let observers_per_update = observer_metrics_urls.len() as u64;
    let network_metrics_urls = vec![
        ctx.node1.metrics_url.clone(),
        ctx.node2.metrics_url.clone(),
        ctx.node3.metrics_url.clone(),
    ];

    println!("🚀 Gossip benchmark test:");
    println!("  - source node: node2");
    println!("  - observed nodes: node1,node3");
    println!("  - workload channels: {}", workload_channels.len());
    for channel in &workload_channels {
        println!(
            "    * {} -> {} | {} ({})",
            channel.source_node, channel.peer_node, channel.channel_id, channel.channel_outpoint
        );
    }
    println!("  - duration: {:?}", duration);
    println!("  - load mode: {}", load_mode);
    println!("  - update interval: {:?}", update_interval);
    println!(
        "  - burst target: {} updates per {:?}",
        burst_updates_per_window, burst_window
    );
    println!(
        "  - metrics endpoints: {}",
        network_metrics_urls.join(", ")
    );

    let baseline_network_metrics = collect_gossip_metrics_per_node(&ctx, &network_metrics_urls).await;
    let baseline_observer_metrics =
        collect_gossip_metrics_per_node(&ctx, &observer_metrics_urls).await;
    if baseline_network_metrics.is_none() || baseline_observer_metrics.is_none() {
        eprintln!("Warning: gossip metrics are unavailable, duplicate/bandwidth KPIs will be 0");
    }

    let start = Instant::now();
    let end = start + duration;
    let mut next_fee_by_channel: HashMap<String, u128> = workload_channels
        .iter()
        .map(|channel| (channel.channel_id.clone(), GOSSIP_UPDATE_FEE_BASE))
        .collect();
    let mut last_update_ms_by_channel: HashMap<String, u64> =
        HashMap::with_capacity(workload_channels.len());
    let mut workload_channel_index = 0usize;

    let mut total_updates = 0u64;
    let mut burst_windows = 0u64;
    let mut burst_updates_sent = 0u64;
    let mut peak_send_updates_per_sec = 0.0f64;

    while Instant::now() < end {
        if burst_updates_per_window > 0 {
            let burst_window_start = Instant::now();
            let mut sent_in_window = 0u64;

            while sent_in_window < burst_updates_per_window && Instant::now() < end {
                trigger_next_gossip_update(
                    &ctx,
                    &mut workload_channels,
                    &mut next_fee_by_channel,
                    &mut last_update_ms_by_channel,
                    &mut workload_channel_index,
                )
                .await?;
                sent_in_window = sent_in_window.saturating_add(1);
                total_updates = total_updates.saturating_add(1);
            }

            burst_windows = burst_windows.saturating_add(1);
            burst_updates_sent = burst_updates_sent.saturating_add(sent_in_window);
            let window_elapsed_secs = burst_window_start.elapsed().as_secs_f64();
            if window_elapsed_secs > 0.0 {
                peak_send_updates_per_sec =
                    peak_send_updates_per_sec.max(sent_in_window as f64 / window_elapsed_secs);
            }

            let elapsed_in_window = burst_window_start.elapsed();
            if elapsed_in_window < burst_window {
                let remaining_in_window = burst_window - elapsed_in_window;
                let remaining_total = end.saturating_duration_since(Instant::now());
                let sleep_duration = remaining_in_window.min(remaining_total);
                if !sleep_duration.is_zero() {
                    sleep(sleep_duration).await;
                }
            }
        } else {
            trigger_next_gossip_update(
                &ctx,
                &mut workload_channels,
                &mut next_fee_by_channel,
                &mut last_update_ms_by_channel,
                &mut workload_channel_index,
            )
            .await?;
            total_updates = total_updates.saturating_add(1);

            let remaining_total = end.saturating_duration_since(Instant::now());
            let sleep_duration = update_interval.min(remaining_total);
            if !sleep_duration.is_zero() {
                sleep(sleep_duration).await;
            }
        }
    }

    let elapsed_secs = start.elapsed().as_secs_f64();

    let final_network_metrics = collect_gossip_metrics_per_node(&ctx, &network_metrics_urls).await;
    let final_observer_metrics = collect_gossip_metrics_per_node(&ctx, &observer_metrics_urls).await;

    let metrics_delta = match (baseline_network_metrics, final_network_metrics) {
        (Some(baseline), Some(current)) => {
            let baseline_total = sum_gossip_metrics(&baseline);
            let current_total = sum_gossip_metrics(&current);
            Some(current_total.saturating_sub(baseline_total))
        }
        _ => None,
    };
    let metrics_collected = metrics_delta.is_some();
    let metrics_delta = metrics_delta.unwrap_or_default();

    let (latency_histogram_delta, observer_seen_updates): (HistogramSnapshot, Vec<u64>) =
        match (baseline_observer_metrics, final_observer_metrics) {
            (Some(baseline), Some(current)) if baseline.len() == current.len() => {
                let mut latency_total = HistogramSnapshot::default();
                let mut seen_updates = Vec::with_capacity(current.len());

                for (baseline_snapshot, current_snapshot) in baseline.iter().zip(current.iter()) {
                    let observer_latency_delta = current_snapshot
                        .applied_channel_update_latency
                        .saturating_sub(&baseline_snapshot.applied_channel_update_latency);
                    latency_total.add_assign(&observer_latency_delta);
                    seen_updates.push(observer_latency_delta.count.min(total_updates));
                }

                (latency_total, seen_updates)
            }
            _ => (HistogramSnapshot::default(), Vec::new()),
        };

    let full_convergence_updates = observer_seen_updates
        .iter()
        .copied()
        .min()
        .unwrap_or_default();
    let total_seen_updates = observer_seen_updates.iter().copied().sum::<u64>();
    let latency_samples = latency_histogram_delta.count;
    let full_convergence_rate_percent = if total_updates == 0 {
        0.0
    } else {
        full_convergence_updates as f64 * 100.0 / total_updates as f64
    };
    let avg_convergence_rate_percent = if total_updates == 0 || observers_per_update == 0 {
        0.0
    } else {
        total_seen_updates as f64 * 100.0 / (total_updates * observers_per_update) as f64
    };

    let duplicate_rate_percent = if metrics_delta.received_broadcast_messages == 0 {
        0.0
    } else {
        metrics_delta.duplicate_broadcast_messages as f64 * 100.0
            / metrics_delta.received_broadcast_messages as f64
    };
    let rejected_rate_percent = if metrics_delta.received_broadcast_messages == 0 {
        0.0
    } else {
        metrics_delta.rejected_broadcast_messages as f64 * 100.0
            / metrics_delta.received_broadcast_messages as f64
    };
    let bytes_per_applied_message = if metrics_delta.applied_broadcast_messages == 0 {
        0.0
    } else {
        (metrics_delta.received_gossip_bytes + metrics_delta.sent_gossip_bytes) as f64
            / metrics_delta.applied_broadcast_messages as f64
    };
    let updates_per_sec = if elapsed_secs > 0.0 {
        total_updates as f64 / elapsed_secs
    } else {
        0.0
    };
    if burst_updates_per_window == 0 {
        peak_send_updates_per_sec = updates_per_sec;
    }

    let result = GossipBenchmarkResult {
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string(),
        test_duration_secs: elapsed_secs,
        update_interval_ms: update_interval.as_millis() as u64,
        load_mode: load_mode.to_string(),
        burst_updates_per_window,
        burst_window_ms: burst_window.as_millis() as u64,
        burst_windows,
        burst_updates_sent,
        peak_send_updates_per_sec,
        total_updates,
        workload_channels: workload_channels.clone(),
        full_convergence_updates,
        full_convergence_rate_percent,
        avg_convergence_rate_percent,
        latency_samples,
        avg_latency_ms: latency_histogram_delta.average(),
        p50_latency_ms: latency_histogram_delta.percentile(0.50),
        p90_latency_ms: latency_histogram_delta.percentile(0.90),
        p99_latency_ms: latency_histogram_delta.percentile(0.99),
        max_latency_ms: latency_histogram_delta.max_latency_bound(),
        latency_le_50ms_percent: latency_histogram_delta.coverage_percent_le(50.0),
        latency_le_100ms_percent: latency_histogram_delta.coverage_percent_le(100.0),
        latency_le_200ms_percent: latency_histogram_delta.coverage_percent_le(200.0),
        latency_le_500ms_percent: latency_histogram_delta.coverage_percent_le(500.0),
        updates_per_sec,
        metrics_collected,
        received_broadcast_messages: metrics_delta.received_broadcast_messages,
        applied_broadcast_messages: metrics_delta.applied_broadcast_messages,
        duplicate_broadcast_messages: metrics_delta.duplicate_broadcast_messages,
        rejected_broadcast_messages: metrics_delta.rejected_broadcast_messages,
        received_gossip_bytes: metrics_delta.received_gossip_bytes,
        sent_gossip_bytes: metrics_delta.sent_gossip_bytes,
        duplicate_rate_percent,
        rejected_rate_percent,
        bytes_per_applied_message,
    };

    println!("\n🎯 Gossip benchmark results:");
    println!("=====================================");
    println!("Test duration: {:.2} seconds", result.test_duration_secs);
    println!(
        "Load profile: mode={}, interval={}ms, burst_target={} updates/{}ms, burst_windows={}",
        result.load_mode,
        result.update_interval_ms,
        result.burst_updates_per_window,
        result.burst_window_ms,
        result.burst_windows
    );
    println!("Scope: observer metrics = node1,node3 (combined)");
    println!("Scope: network counters = node1,node2,node3 (sum delta)");
    println!(
        "Latency source: {} histogram (percentiles are estimates)",
        GOSSIP_APPLIED_LATENCY_METRIC
    );
    println!("Total updates: {}", result.total_updates);
    println!("Updates/sec: {:.2}", result.updates_per_sec);
    println!(
        "Pressure send rate: peak={:.2} updates/sec, burst_updates_sent={}",
        result.peak_send_updates_per_sec, result.burst_updates_sent
    );
    println!("Workload distribution:");
    for channel in &result.workload_channels {
        println!(
            "  - {} -> {} | channel={} | updates={}",
            channel.source_node, channel.peer_node, channel.channel_id, channel.updates
        );
    }
    println!(
        "Full convergence: {}/{} ({:.2}%)",
        result.full_convergence_updates, result.total_updates, result.full_convergence_rate_percent
    );
    println!(
        "Average convergence rate: {:.2}%",
        result.avg_convergence_rate_percent
    );
    println!(
        "Latency(ms): avg={:.2}, p50={:.2}, p90={:.2}, p99={:.2}, max={:.2}",
        result.avg_latency_ms,
        result.p50_latency_ms,
        result.p90_latency_ms,
        result.p99_latency_ms,
        result.max_latency_ms
    );
    println!(
        "Latency coverage: <=50ms {:.2}%, <=100ms {:.2}%, <=200ms {:.2}%, <=500ms {:.2}%",
        result.latency_le_50ms_percent,
        result.latency_le_100ms_percent,
        result.latency_le_200ms_percent,
        result.latency_le_500ms_percent
    );
    if result.metrics_collected {
        println!(
            "Metrics deltas: recv_msg={}, applied={}, dup={}, rejected={}, recv_bytes={}, sent_bytes={}",
            result.received_broadcast_messages,
            result.applied_broadcast_messages,
            result.duplicate_broadcast_messages,
            result.rejected_broadcast_messages,
            result.received_gossip_bytes,
            result.sent_gossip_bytes
        );
        println!(
            "Derived: duplicate_rate={:.2}%, rejected_rate={:.2}%, bytes_per_applied={:.2}",
            result.duplicate_rate_percent, result.rejected_rate_percent, result.bytes_per_applied_message
        );
    }
    println!("=====================================\n");

    Ok(result)
}

/// Save benchmark result to a JSON file
pub fn save_benchmark_baseline(result: &BenchmarkResult, filename: &str) -> TestResult<()> {
    let json_data = serde_json::to_string_pretty(result)?;
    fs::write(filename, json_data)?;
    println!("📊 Baseline saved to: {}", filename);
    Ok(())
}

/// Load benchmark baseline from a JSON file
pub fn load_benchmark_baseline(filename: &str) -> TestResult<BenchmarkResult> {
    let json_data = fs::read_to_string(filename)?;
    let result: BenchmarkResult = serde_json::from_str(&json_data)?;
    Ok(result)
}

/// Compare current benchmark result with baseline
pub fn compare_with_baseline(
    current: &BenchmarkResult,
    baseline: &BenchmarkResult,
    filename: &str,
) -> TestResult<()> {
    let mut output = String::new();

    // Helper macro to write to both console and string buffer
    macro_rules! write_line {
        ($($arg:tt)*) => {
            let line = format!($($arg)*);
            println!("{}", line);
            output.push_str(&line);
            output.push('\n');
        };
    }

    write_line!("\n🔄 Benchmark Comparison Results:");
    write_line!("=====================================");

    // TPS comparison
    let tps_diff = current.tps - baseline.tps;
    let tps_percent = (tps_diff / baseline.tps) * 100.0;
    write_line!(
        "TPS: {:.2} (baseline) vs {:.2} (current) | Diff: {:+.2} ({:+.1}%)",
        baseline.tps,
        current.tps,
        tps_diff,
        tps_percent
    );

    // Success rate comparison
    let success_diff = current.success_rate_percent - baseline.success_rate_percent;
    write_line!(
        "Success Rate: {:.2}% (baseline) vs {:.2}% (current) | Diff: {:+.2}%",
        baseline.success_rate_percent,
        current.success_rate_percent,
        success_diff
    );

    // Latency comparison
    let latency_diff = current.avg_latency_ms - baseline.avg_latency_ms;
    let latency_percent = (latency_diff / baseline.avg_latency_ms) * 100.0;
    write_line!(
        "Avg Latency: {:.2}ms (baseline) vs {:.2}ms (current) | Diff: {:+.2}ms ({:+.1}%)",
        baseline.avg_latency_ms,
        current.avg_latency_ms,
        latency_diff,
        latency_percent
    );

    let mut failed = false;
    // Overall assessment
    write_line!("\n📈 Performance Assessment:");
    if tps_percent > 5.0 {
        write_line!("✅ TPS improved significantly (+{:.1}%)", tps_percent);
    } else if tps_percent > 0.0 {
        write_line!("🟡 TPS slightly improved (+{:.1}%)", tps_percent);
    } else if tps_percent > -5.0 {
        write_line!("🟡 TPS slightly decreased ({:.1}%)", tps_percent);
    } else {
        write_line!("❌ TPS significantly decreased ({:.1}%)", tps_percent);
        failed = true;
    }

    if success_diff > 1.0 {
        write_line!("✅ Success rate improved (+{:.2}%)", success_diff);
    } else if success_diff > -5.0 {
        write_line!("🟡 Success rate stable ({:+.2}%)", success_diff);
    } else {
        write_line!("❌ Success rate decreased ({:.2}%)", success_diff);
        failed = true;
    }

    write_line!("=====================================\n");
    if failed {
        write_line!("❗ Performance regression detected!");
    }
    // Write to file
    fs::write(filename, output)?;
    println!("📁 Comparison results saved to: {}", filename);

    Ok(())
}

/// Save gossip benchmark result to a JSON file
pub fn save_gossip_benchmark_baseline(
    result: &GossipBenchmarkResult,
    filename: &str,
) -> TestResult<()> {
    let json_data = serde_json::to_string_pretty(result)?;
    fs::write(filename, json_data)?;
    println!("📊 Gossip baseline saved to: {}", filename);
    Ok(())
}

/// Load gossip benchmark baseline from a JSON file
pub fn load_gossip_benchmark_baseline(filename: &str) -> TestResult<GossipBenchmarkResult> {
    let json_data = fs::read_to_string(filename)?;
    let result: GossipBenchmarkResult = serde_json::from_str(&json_data)?;
    Ok(result)
}

/// Compare current gossip benchmark result with baseline.
pub fn compare_gossip_with_baseline(
    current: &GossipBenchmarkResult,
    baseline: &GossipBenchmarkResult,
    filename: &str,
) -> TestResult<()> {
    let mut output = String::new();
    let mut failed = false;

    macro_rules! write_line {
        ($($arg:tt)*) => {
            let line = format!($($arg)*);
            println!("{}", line);
            output.push_str(&line);
            output.push('\n');
        };
    }

    let updates_per_sec_diff = current.updates_per_sec - baseline.updates_per_sec;
    let updates_per_sec_percent = if baseline.updates_per_sec > 0.0 {
        updates_per_sec_diff * 100.0 / baseline.updates_per_sec
    } else {
        0.0
    };
    let p90_diff = current.p90_latency_ms - baseline.p90_latency_ms;
    let p90_percent = if baseline.p90_latency_ms > 0.0 {
        p90_diff * 100.0 / baseline.p90_latency_ms
    } else {
        0.0
    };
    let convergence_diff =
        current.avg_convergence_rate_percent - baseline.avg_convergence_rate_percent;
    let duplicate_rate_diff = current.duplicate_rate_percent - baseline.duplicate_rate_percent;
    let rejected_rate_diff = current.rejected_rate_percent - baseline.rejected_rate_percent;
    let bytes_per_applied_diff =
        current.bytes_per_applied_message - baseline.bytes_per_applied_message;
    let latency_le_200_diff = current.latency_le_200ms_percent - baseline.latency_le_200ms_percent;
    let peak_send_rate_diff = current.peak_send_updates_per_sec - baseline.peak_send_updates_per_sec;

    write_line!("\n🔄 Gossip Benchmark Comparison Results:");
    write_line!("=====================================");
    write_line!("Scope: observer metrics = node1,node3 (combined)");
    write_line!("Scope: network counters = node1,node2,node3 (sum delta)");
    write_line!(
        "Latency source: {} histogram (percentiles are estimates)",
        GOSSIP_APPLIED_LATENCY_METRIC
    );
    write_line!(
        "Load profile: baseline={} (interval={}ms, burst={} per {}ms) | current={} (interval={}ms, burst={} per {}ms)",
        baseline.load_mode,
        baseline.update_interval_ms,
        baseline.burst_updates_per_window,
        baseline.burst_window_ms,
        current.load_mode,
        current.update_interval_ms,
        current.burst_updates_per_window,
        current.burst_window_ms
    );
    write_line!(
        "Updates/sec: {:.2} (baseline) vs {:.2} (current) | Diff: {:+.2} ({:+.1}%)",
        baseline.updates_per_sec,
        current.updates_per_sec,
        updates_per_sec_diff,
        updates_per_sec_percent
    );
    write_line!(
        "P90 Latency: {:.2}ms (baseline) vs {:.2}ms (current) | Diff: {:+.2}ms ({:+.1}%)",
        baseline.p90_latency_ms,
        current.p90_latency_ms,
        p90_diff,
        p90_percent
    );
    write_line!(
        "Avg Convergence: {:.2}% (baseline) vs {:.2}% (current) | Diff: {:+.2}%",
        baseline.avg_convergence_rate_percent,
        current.avg_convergence_rate_percent,
        convergence_diff
    );
    write_line!(
        "Duplicate Rate: {:.2}% (baseline) vs {:.2}% (current) | Diff: {:+.2}%",
        baseline.duplicate_rate_percent,
        current.duplicate_rate_percent,
        duplicate_rate_diff
    );
    write_line!(
        "Rejected Rate: {:.2}% (baseline) vs {:.2}% (current) | Diff: {:+.2}%",
        baseline.rejected_rate_percent,
        current.rejected_rate_percent,
        rejected_rate_diff
    );
    write_line!(
        "Bytes/Applied: {:.2} (baseline) vs {:.2} (current) | Diff: {:+.2}",
        baseline.bytes_per_applied_message,
        current.bytes_per_applied_message,
        bytes_per_applied_diff
    );
    write_line!(
        "Latency <=200ms: {:.2}% (baseline) vs {:.2}% (current) | Diff: {:+.2}%",
        baseline.latency_le_200ms_percent,
        current.latency_le_200ms_percent,
        latency_le_200_diff
    );
    write_line!(
        "Peak send rate: {:.2} (baseline) vs {:.2} (current) updates/sec | Diff: {:+.2}",
        baseline.peak_send_updates_per_sec,
        current.peak_send_updates_per_sec,
        peak_send_rate_diff
    );

    if baseline.load_mode != current.load_mode
        || baseline.burst_updates_per_window != current.burst_updates_per_window
        || baseline.burst_window_ms != current.burst_window_ms
        || baseline.update_interval_ms != current.update_interval_ms
    {
        write_line!(
            "⚠️ Baseline/current load profiles differ; throughput and latency deltas may not be directly comparable"
        );
    }

    write_line!("\n📈 Gossip Performance Assessment:");
    if updates_per_sec_percent < -10.0 {
        write_line!(
            "❌ Updates/sec decreased significantly ({:.1}%)",
            updates_per_sec_percent
        );
        failed = true;
    } else {
        write_line!(
            "✅ Updates/sec is within threshold ({:+.1}%)",
            updates_per_sec_percent
        );
    }

    if p90_percent > 20.0 {
        write_line!(
            "❌ P90 latency regressed significantly (+{:.1}%)",
            p90_percent
        );
        failed = true;
    } else {
        write_line!("✅ P90 latency is within threshold ({:+.1}%)", p90_percent);
    }

    if convergence_diff < -1.0 {
        write_line!(
            "❌ Average convergence rate dropped significantly ({:.2}%)",
            convergence_diff
        );
        failed = true;
    } else {
        write_line!(
            "✅ Average convergence rate is within threshold ({:+.2}%)",
            convergence_diff
        );
    }

    if latency_le_200_diff < -5.0 {
        write_line!(
            "❌ Latency <=200ms coverage regressed significantly ({:.2}%)",
            latency_le_200_diff
        );
        failed = true;
    } else {
        write_line!(
            "✅ Latency <=200ms coverage is within threshold ({:+.2}%)",
            latency_le_200_diff
        );
    }

    if baseline.metrics_collected && current.metrics_collected {
        let duplicate_regression_percent = if baseline.duplicate_rate_percent > 0.0 {
            duplicate_rate_diff * 100.0 / baseline.duplicate_rate_percent
        } else if current.duplicate_rate_percent > 0.0 {
            100.0
        } else {
            0.0
        };

        if duplicate_regression_percent > 15.0 {
            write_line!(
                "❌ Duplicate rate regressed significantly (+{:.1}%)",
                duplicate_regression_percent
            );
            failed = true;
        } else {
            write_line!(
                "✅ Duplicate rate is within threshold ({:+.1}%)",
                duplicate_regression_percent
            );
        }

        let rejected_regression_percent = if baseline.rejected_rate_percent > 0.0 {
            rejected_rate_diff * 100.0 / baseline.rejected_rate_percent
        } else if current.rejected_rate_percent > 0.0 {
            100.0
        } else {
            0.0
        };
        if rejected_regression_percent > 15.0 {
            write_line!(
                "❌ Rejected rate regressed significantly (+{:.1}%)",
                rejected_regression_percent
            );
            failed = true;
        } else {
            write_line!(
                "✅ Rejected rate is within threshold ({:+.1}%)",
                rejected_regression_percent
            );
        }

        let bytes_per_applied_regression_percent = if baseline.bytes_per_applied_message > 0.0 {
            bytes_per_applied_diff * 100.0 / baseline.bytes_per_applied_message
        } else if current.bytes_per_applied_message > 0.0 {
            100.0
        } else {
            0.0
        };
        if bytes_per_applied_regression_percent > 15.0 {
            write_line!(
                "❌ Bytes per applied regressed significantly (+{:.1}%)",
                bytes_per_applied_regression_percent
            );
            failed = true;
        } else {
            write_line!(
                "✅ Bytes per applied is within threshold ({:+.1}%)",
                bytes_per_applied_regression_percent
            );
        }
    } else {
        write_line!(
            "⚠️ Metrics counters are unavailable in baseline/current, skipping duplicate/rejected/bandwidth regression checks"
        );
    }

    write_line!("=====================================\n");
    if failed {
        write_line!("❗ Performance regression detected!");
    }

    fs::write(filename, output)?;
    println!("📁 Gossip comparison results saved to: {}", filename);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_f64_close(actual: f64, expected: f64, epsilon: f64) {
        let diff = (actual - expected).abs();
        assert!(
            diff <= epsilon,
            "expected {expected}, got {actual}, diff {diff}, epsilon {epsilon}"
        );
    }

    #[tokio::test]
    async fn test_connect_peers() -> TestResult<()> {
        let ctx = TestContext::new();
        ctx.connect_peer(&ctx.node2.rpc_url, &ctx.node1.addr)
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_open_channel() -> TestResult<()> {
        let ctx = TestContext::new();
        // First connect peers
        ctx.connect_peer(&ctx.node2.rpc_url, &ctx.node1.addr)
            .await?;
        // Then open channel
        let _temp_id = ctx
            .open_channel(&ctx.node1.rpc_url, &ctx.node2.pubkey, "0x377aab54d000")
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_list_channels() -> TestResult<()> {
        let ctx = TestContext::new();
        let _result = ctx
            .list_channels(&ctx.node1.rpc_url, Some(&ctx.node2.pubkey))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_send_payment() -> TestResult<()> {
        let ctx = TestContext::new();
        let result = ctx
            .send_payment(&ctx.node1.rpc_url, NODE_3_PUBKEY, "0x1F4", true)
            .await?;

        let payment_hash = result["result"]["payment_hash"]
            .as_str()
            .ok_or_else(|| TestError::Assertion("Expected payment_hash".to_string()))?;

        loop {
            let is_success = ctx
                .is_payment_success(&ctx.node1.rpc_url, payment_hash)
                .await?;
            if is_success {
                println!("Payment succeeded with hash: {}", payment_hash);
                break;
            } else {
                println!("Payment not yet succeeded, retrying...");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_full_integration() -> TestResult<()> {
        run_integration_test().await
    }

    #[test]
    #[ignore]
    fn test_benchmark() -> TestResult<()> {
        let _result = run_benchmark_test(Duration::from_secs(180), 6)?;
        Ok(())
    }

    #[test]
    #[ignore]
    fn test_short_benchmark() -> TestResult<()> {
        let _result = run_benchmark_test(Duration::from_secs(30), 2)?;
        Ok(())
    }

    #[test]
    fn test_parse_prometheus_metric_sum_accepts_dotted_and_sanitized_names() {
        let text = r#"
fiber.gossip.received_bytes_total 12
fiber_gossip_received_bytes_total{node="node1"} 8
fiber_gossip_received_bytes_total{node="node2"} -1
fiber.gossip.received_broadcast_messages_total 5
"#;
        let value = parse_prometheus_metric_sum(text, "fiber.gossip.received_bytes_total");
        assert_eq!(value, 20);
    }

    #[test]
    fn test_parse_prometheus_histogram_filters_message_type() {
        let text = r#"
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="10"} 2
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="50"} 4
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="+Inf"} 5
fiber_gossip_propagation_applied_latency_ms_sum{message_type="channel_update"} 93
fiber_gossip_propagation_applied_latency_ms_count{message_type="channel_update"} 5
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="node_announcement",le="10"} 7
fiber_gossip_propagation_applied_latency_ms_count{message_type="node_announcement"} 7
"#;

        let histogram = parse_prometheus_histogram(
            text,
            GOSSIP_APPLIED_LATENCY_METRIC,
            &[("message_type", CHANNEL_UPDATE_MESSAGE_TYPE)],
        );
        assert_eq!(histogram.count, 5);
        assert_f64_close(histogram.sum, 93.0, 1e-9);
        assert_eq!(histogram.buckets.len(), 3);
        assert_eq!(histogram.buckets[0], (10.0, 2));
        assert_eq!(histogram.buckets[1], (50.0, 4));
        assert_eq!(histogram.buckets[2].0, f64::INFINITY);
        assert_eq!(histogram.buckets[2].1, 5);
    }

    #[test]
    fn test_histogram_snapshot_delta_and_percentiles() {
        let baseline_text = r#"
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="10"} 1
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="50"} 2
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="+Inf"} 2
fiber_gossip_propagation_applied_latency_ms_sum{message_type="channel_update"} 20
fiber_gossip_propagation_applied_latency_ms_count{message_type="channel_update"} 2
"#;
        let current_text = r#"
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="10"} 2
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="50"} 4
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="+Inf"} 5
fiber_gossip_propagation_applied_latency_ms_sum{message_type="channel_update"} 93
fiber_gossip_propagation_applied_latency_ms_count{message_type="channel_update"} 5
"#;

        let baseline = parse_prometheus_histogram(
            baseline_text,
            GOSSIP_APPLIED_LATENCY_METRIC,
            &[("message_type", CHANNEL_UPDATE_MESSAGE_TYPE)],
        );
        let current = parse_prometheus_histogram(
            current_text,
            GOSSIP_APPLIED_LATENCY_METRIC,
            &[("message_type", CHANNEL_UPDATE_MESSAGE_TYPE)],
        );

        let delta = current.saturating_sub(&baseline);
        assert_eq!(delta.count, 3);
        assert_f64_close(delta.sum, 73.0, 1e-9);
        assert_eq!(delta.buckets, vec![(10.0, 1), (50.0, 2), (f64::INFINITY, 3)]);
        assert_f64_close(delta.average(), 73.0 / 3.0, 1e-9);
        assert_f64_close(delta.percentile(0.50), 50.0, 1e-9);
        assert_f64_close(delta.percentile(0.90), 50.0, 1e-9);
        assert_f64_close(delta.max_latency_bound(), 50.0, 1e-9);
    }

    #[test]
    fn test_histogram_coverage_percent_le() {
        let text = r#"
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="50"} 2
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="200"} 8
fiber_gossip_propagation_applied_latency_ms_bucket{message_type="channel_update",le="+Inf"} 10
fiber_gossip_propagation_applied_latency_ms_sum{message_type="channel_update"} 800
fiber_gossip_propagation_applied_latency_ms_count{message_type="channel_update"} 10
"#;
        let histogram = parse_prometheus_histogram(
            text,
            GOSSIP_APPLIED_LATENCY_METRIC,
            &[("message_type", CHANNEL_UPDATE_MESSAGE_TYPE)],
        );

        assert_f64_close(histogram.coverage_percent_le(50.0), 20.0, 1e-9);
        assert_f64_close(histogram.coverage_percent_le(200.0), 80.0, 1e-9);
        assert_f64_close(histogram.coverage_percent_le(500.0), 100.0, 1e-9);
    }

    #[test]
    fn test_should_wait_for_next_channel_update_millis_when_timestamp_not_advanced() {
        assert!(should_wait_for_next_channel_update_millis(Some(1_000), 1_000));
        assert!(should_wait_for_next_channel_update_millis(Some(1_000), 999));
        assert!(!should_wait_for_next_channel_update_millis(Some(1_000), 1_001));
    }

    #[test]
    fn test_should_not_wait_for_first_channel_update_millis() {
        assert!(!should_wait_for_next_channel_update_millis(None, 0));
        assert!(!should_wait_for_next_channel_update_millis(None, 1_000));
    }
}
