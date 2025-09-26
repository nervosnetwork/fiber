use regex::Regex;
use reqwest::{Client, Error as ReqwestError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::time::sleep;

const NODE_3_PUBKEY: &str = "03032b99943822e721a651c5a5b9621043017daa9dc3ec81d83215fd2e25121187";

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
    pub peer_id: String,
    pub addr: String,
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

    /// Create TestContext from parsed configuration variables
    fn from_config_vars(client: Client, vars: HashMap<String, String>) -> Self {
        Self {
            client,
            node1: NodeConfig {
                rpc_url: Self::get_config_value(&vars, "NODE1_RPC_URL"),
                peer_id: Self::get_config_value(&vars, "NODE1_PEERID"),
                addr: Self::get_config_value(&vars, "NODE1_ADDR"),
            },
            node2: NodeConfig {
                rpc_url: Self::get_config_value(&vars, "NODE2_RPC_URL"),
                peer_id: Self::get_config_value(&vars, "NODE2_PEERID"),
                addr: Self::get_config_value(&vars, "NODE2_ADDR"),
            },
            node3: NodeConfig {
                rpc_url: Self::get_config_value(&vars, "NODE3_RPC_URL"),
                peer_id: Self::get_config_value(&vars, "NODE3_PEERID"),
                addr: Self::get_config_value(&vars, "NODE3_ADDR"),
            },
            ckb_rpc_url: Self::get_config_value(&vars, "CKB_RPC_URL"),
            udt_code_hash: Self::get_config_value(&vars, "UDT_CODE_HASH"),
            lnd_bob_rpc_url: Self::get_config_value(&vars, "LND_BOB_RPC_URL"),
            lnd_ingrid_rpc_url: Self::get_config_value(&vars, "LND_INGRID_RPC_URL"),
            n1n2_channel_id: None,
            n2n3_channel_id: None,
        }
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
        peer_id: &str,
        funding_amount: &str,
    ) -> TestResult<String> {
        let params = json!([{
            "peer_id": peer_id,
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
    pub async fn list_channels(&self, node_url: &str, peer_id: Option<&str>) -> TestResult<Value> {
        let params = if let Some(peer_id) = peer_id {
            json!([{"peer_id": peer_id}])
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
        peer_id: &str,
    ) -> TestResult<String> {
        let result = self.list_channels(node_url, Some(peer_id)).await?;

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
        peer_id: &str,
        expected_remote: &str,
        expected_local: &str,
    ) -> TestResult<()> {
        let result = self.list_channels(node_url, Some(peer_id)).await?;

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

    println!("🔗 Step 1: Connecting peers...");
    ctx.connect_peer(&ctx.node2.rpc_url, &ctx.node1.addr)
        .await?;
    ctx.connect_peer(&ctx.node3.rpc_url, &ctx.node2.addr)
        .await?;

    println!("📡 Step 2: Opening channels...");
    let _temp_channel_1 = ctx
        .open_channel(&ctx.node1.rpc_url, &ctx.node2.peer_id, "0x377aab54d000")
        .await?;
    ctx.n1n2_channel_id = Some(
        ctx.get_auto_accepted_channel(&ctx.node2.rpc_url, &ctx.node1.peer_id)
            .await?,
    );

    println!("⛏️ Step 3: Generating blocks for channel 1...");
    ctx.generate_ckb_epochs("0x2").await?;

    let _temp_channel_2 = ctx
        .open_channel(&ctx.node2.rpc_url, &ctx.node3.peer_id, "0x377aab54d000")
        .await?;
    ctx.n2n3_channel_id = Some(
        ctx.get_auto_accepted_channel(&ctx.node3.rpc_url, &ctx.node2.peer_id)
            .await?,
    );

    println!("⛏️ Step 4: Generating blocks for channel 2...");
    ctx.generate_ckb_epochs("0x2").await?;

    println!("✅ Step 5: Validating channel balances...");
    ctx.validate_channel_balance(
        &ctx.node2.rpc_url,
        &ctx.node3.peer_id,
        "0x0",
        "0x377939c85200",
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
        "TPS: {:.2} vs {:.2} (baseline) | Diff: {:+.2} ({:+.1}%)",
        current.tps,
        baseline.tps,
        tps_diff,
        tps_percent
    );

    // Success rate comparison
    let success_diff = current.success_rate_percent - baseline.success_rate_percent;
    write_line!(
        "Success Rate: {:.2}% vs {:.2}% (baseline) | Diff: {:+.2}%",
        current.success_rate_percent,
        baseline.success_rate_percent,
        success_diff
    );

    // Latency comparison
    let latency_diff = current.avg_latency_ms - baseline.avg_latency_ms;
    let latency_percent = (latency_diff / baseline.avg_latency_ms) * 100.0;
    write_line!(
        "Avg Latency: {:.2}ms vs {:.2}ms (baseline) | Diff: {:+.2}ms ({:+.1}%)",
        current.avg_latency_ms,
        baseline.avg_latency_ms,
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

#[cfg(test)]
mod tests {
    use super::*;

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
            .open_channel(&ctx.node1.rpc_url, &ctx.node2.peer_id, "0x377aab54d000")
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn test_list_channels() -> TestResult<()> {
        let ctx = TestContext::new();
        let _result = ctx
            .list_channels(&ctx.node1.rpc_url, Some(&ctx.node2.peer_id))
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
}
