//! Independent local CKB verifier for the dev-chain E2E fixture.
use anyhow::{ensure, Context, Result};
use ckb_types::{
    core::EpochNumberWithFraction,
    packed::{CellDep, CellInput, OutPoint},
    prelude::*,
};
use fiber_lsp_sdk::{ChainVerifier, SignerError, VerifiedCell};
use fiber_types::Hash256;
use serde::de::DeserializeOwned;
use serde_json::json;

/// Trusts the separately configured CKB node, never the hosted LSP endpoint.
#[derive(Clone)]
pub struct DevChain {
    url: String,
    client: reqwest::Client,
    funding_deps: Option<Vec<CellDep>>,
}

impl DevChain {
    /// Connect to the test driver's independent chain source.
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            url: url.into(),
            funding_deps: None,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
        })
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let body: serde_json::Value = self
            .client
            .post(&self.url)
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            body.get("error").is_none(),
            "CKB {method}: {}",
            body["error"]
        );
        Ok(serde_json::from_value(body["result"].clone())?)
    }

    async fn committed(
        &self,
        hash: &ckb_types::packed::Byte32,
    ) -> Result<(
        ckb_jsonrpc_types::TransactionView,
        ckb_jsonrpc_types::HeaderView,
    )> {
        let hash: ckb_types::H256 = hash.unpack();
        let result: serde_json::Value = self.call("get_transaction", json!([hash])).await?;
        ensure!(
            result["tx_status"]["status"] == "committed",
            "transaction is not committed"
        );
        let tx: ckb_jsonrpc_types::TransactionView =
            serde_json::from_value(result["transaction"].clone())?;
        let header: ckb_jsonrpc_types::HeaderView = self
            .call("get_header", json!([result["tx_status"]["block_hash"]]))
            .await?;
        let canonical: ckb_jsonrpc_types::HeaderView = self
            .call("get_header_by_number", json!([header.inner.number]))
            .await?;
        ensure!(
            canonical.hash == header.hash,
            "transaction was reorganized out"
        );
        let packed: ckb_types::packed::Transaction = tx.inner.clone().into();
        ensure!(
            packed.calc_tx_hash() == hash.pack() && tx.hash == hash,
            "transaction hash mismatch"
        );
        Ok((tx, header))
    }

    /// Exact dev-chain dependencies obtained independently from genesis and local binaries.
    pub async fn settlement_deps(&mut self, contracts: &std::path::Path) -> Result<Vec<CellDep>> {
        let genesis: ckb_jsonrpc_types::BlockView =
            self.call("get_block_by_number", json!(["0x0"])).await?;
        let cells = genesis
            .transactions
            .first()
            .context("missing genesis cells")?;
        for (index, name) in [(5, "auth"), (6, "funding-lock"), (7, "commitment-lock")] {
            ensure!(
                cells
                    .inner
                    .outputs_data
                    .get(index)
                    .context("missing contract")?
                    .as_bytes()
                    == std::fs::read(contracts.join(name))?,
                "genesis contract differs from local {name}"
            );
        }
        let dep = |hash: ckb_types::H256, index: u32, kind: u8| {
            CellDep::new_builder()
                .out_point(
                    OutPoint::new_builder()
                        .tx_hash(hash.pack())
                        .index(index)
                        .build(),
                )
                .dep_type(kind)
                .build()
        };
        self.funding_deps = Some(vec![
            dep(cells.hash.clone(), 6, 0),
            dep(cells.hash.clone(), 5, 0),
        ]);
        Ok(vec![
            dep(cells.hash.clone(), 7, 0),
            dep(cells.hash.clone(), 5, 0),
            dep(
                genesis
                    .transactions
                    .get(1)
                    .context("missing genesis dep group")?
                    .hash
                    .clone(),
                0,
                1,
            ),
        ])
    }

    async fn lineage(&self, source: Hash256, outpoint: &OutPoint) -> Result<()> {
        ensure!(
            outpoint.index() == 0u32.pack(),
            "commitment must be output zero"
        );
        let mut current = outpoint.clone();
        // A fixture cap bounds RPC work. Longer histories fail closed.
        for _ in 0..128 {
            let (tx, _) = self.committed(&current.tx_hash()).await?;
            let packed: ckb_types::packed::Transaction = tx.inner.clone().into();
            if matches_commitment(source, &packed, self.funding_deps.as_deref()) {
                return Ok(());
            }
            let output = tx.inner.outputs.first().context("missing descendant")?;
            let parent: OutPoint = tx
                .inner
                .inputs
                .first()
                .context("missing ancestor")?
                .previous_output
                .clone()
                .into();
            ensure!(
                parent.index() == 0u32.pack(),
                "ancestor must be output zero"
            );
            let (previous, _) = self.committed(&parent.tx_hash()).await?;
            let previous = previous
                .inner
                .outputs
                .first()
                .context("missing ancestor output")?;
            let args = output.lock.args.as_bytes();
            let previous_args = previous.lock.args.as_bytes();
            ensure!(
                output.lock.code_hash == previous.lock.code_hash
                    && output.lock.hash_type == previous.lock.hash_type
                    && output.type_ == previous.type_
                    && args.len() == 57
                    && previous_args.len() == 57
                    && args[..36] == previous_args[..36]
                    && args[56] == 1,
                "not a commitment descendant"
            );
            current = parent;
        }
        anyhow::bail!("commitment ancestry exceeds fixture limit")
    }

    async fn maturity(&self, inputs: &[CellInput]) -> Result<()> {
        let tip: ckb_jsonrpc_types::HeaderView = self.call("get_tip_header", json!([])).await?;
        let median: ckb_jsonrpc_types::Uint64 = self
            .call("get_block_median_time", json!([tip.hash]))
            .await?;
        for input in inputs {
            let since: u64 = input.since().unpack();
            if since == 0 {
                continue;
            }
            ensure!(since & 0x1f00_0000_0000_0000 == 0, "invalid since flags");
            let relative = since >> 63 != 0;
            let value = since & 0x00ff_ffff_ffff_ffff;
            let origin = if relative {
                Some(self.committed(&input.previous_output().tx_hash()).await?.1)
            } else {
                None
            };
            let mature = match (since >> 61) & 3 {
                0 => origin
                    .as_ref()
                    .map_or(Some(value), |h| h.inner.number.value().checked_add(value))
                    .is_some_and(|required| tip.inner.number.value() >= required),
                1 => epoch_mature(
                    tip.inner.epoch.value(),
                    origin.as_ref().map(|h| h.inner.epoch.value()),
                    value,
                )?,
                2 => {
                    // No relative timestamp inputs are generated by this fixture.
                    ensure!(
                        !relative,
                        "relative timestamp since unsupported by dev fixture"
                    );
                    value
                        .checked_mul(1000)
                        .is_some_and(|required| median.value() >= required)
                }
                _ => anyhow::bail!("invalid since metric"),
            };
            ensure!(mature, "input since is not mature");
        }
        Ok(())
    }
}

// Fiber signs the raw transaction with cell_deps cleared, and installs current
// funding deps at broadcast. Resolve that signed template against a committed
// transaction, checking the independently approved deps as well as its message.
fn matches_commitment(
    source: Hash256,
    tx: &ckb_types::packed::Transaction,
    funding_deps: Option<&[CellDep]>,
) -> bool {
    if tx.calc_tx_hash() == ckb_types::packed::Byte32::from(source) {
        return true;
    }
    Hash256::from(fiber_types::compute_tx_message(tx)) == source
        && funding_deps
            .is_some_and(|deps| tx.raw().cell_deps().into_iter().collect::<Vec<_>>() == deps)
}

fn epoch_mature(tip: u64, origin: Option<u64>, delay: u64) -> Result<bool> {
    let fraction = |value| -> Result<(u128, u128)> {
        let epoch = EpochNumberWithFraction::from_full_value(value);
        ensure!(
            epoch.length() > 0 && epoch.index() < epoch.length(),
            "invalid epoch fraction"
        );
        let den = u128::from(epoch.length());
        Ok((
            u128::from(epoch.number()) * den + u128::from(epoch.index()),
            den,
        ))
    };
    let (t, td) = fraction(tip)?;
    let (d, dd) = fraction(delay)?;
    let (o, od) = origin.map(fraction).transpose()?.unwrap_or((0, 1));
    Ok(t * dd * od >= (d * od + o * dd) * td)
}

fn invalid(error: impl std::fmt::Display) -> SignerError {
    SignerError::InvalidContent(format!("independent chain verification: {error}"))
}

#[async_trait::async_trait]
impl ChainVerifier for DevChain {
    async fn live_cell(&self, outpoint: &OutPoint) -> Result<VerifiedCell, SignerError> {
        let point: ckb_jsonrpc_types::OutPoint = outpoint.clone().into();
        let result: ckb_jsonrpc_types::CellWithStatus = self
            .call("get_live_cell", json!([point, true]))
            .await
            .map_err(invalid)?;
        if result.status != "live" {
            return Err(invalid("input is not live"));
        }
        let cell = result.cell.ok_or_else(|| invalid("missing live cell"))?;
        Ok(VerifiedCell {
            output: cell.output.into(),
            data: cell
                .data
                .ok_or_else(|| invalid("missing cell data"))?
                .content
                .into_bytes()
                .to_vec(),
        })
    }
    async fn verify_commitment_lineage(
        &self,
        source: Hash256,
        outpoint: &OutPoint,
    ) -> Result<(), SignerError> {
        self.lineage(source, outpoint).await.map_err(invalid)
    }
    async fn verify_maturity(&self, inputs: &[CellInput]) -> Result<(), SignerError> {
        self.maturity(inputs).await.map_err(invalid)
    }
    async fn median_time_ms(&self) -> Result<u64, SignerError> {
        let tip: ckb_jsonrpc_types::HeaderView = self
            .call("get_tip_header", json!([]))
            .await
            .map_err(invalid)?;
        let time: ckb_jsonrpc_types::Uint64 = self
            .call("get_block_median_time", json!([tip.hash]))
            .await
            .map_err(invalid)?;
        Ok(time.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_epoch_maturity() {
        let epoch = |n, i, l| EpochNumberWithFraction::new(n, i, l).full_value();
        assert!(!epoch_mature(epoch(4, 1, 4), Some(epoch(3, 1, 2)), epoch(0, 5, 6)).unwrap());
        assert!(epoch_mature(epoch(4, 1, 3), Some(epoch(3, 1, 2)), epoch(0, 5, 6)).unwrap());
        assert!(epoch_mature(epoch(1, 3, 2), None, 0).is_err());
    }
    #[test]
    fn broadcast_dependencies_preserve_signed_commitment_identity() {
        let signed = ckb_types::core::TransactionBuilder::default()
            .input(CellInput::default())
            .output(ckb_types::packed::CellOutput::default())
            .output_data(ckb_types::packed::Bytes::default())
            .build()
            .data();
        let source = signed.calc_tx_hash().into();
        let deps = vec![CellDep::new_builder()
            .out_point(OutPoint::new_builder().index(6u32).build())
            .build()];
        let broadcast = signed
            .clone()
            .as_builder()
            .raw(signed.raw().as_builder().cell_deps(deps.clone()).build())
            .build();
        assert_ne!(signed.calc_tx_hash(), broadcast.calc_tx_hash());
        assert!(matches_commitment(source, &broadcast, Some(&deps)));
        assert!(!matches_commitment(source, &broadcast, None));
        assert!(!matches_commitment(source, &broadcast, Some(&[])));
        let altered = broadcast
            .clone()
            .as_builder()
            .raw(
                broadcast
                    .raw()
                    .as_builder()
                    .outputs(vec![ckb_types::packed::CellOutput::new_builder()
                        .capacity(1u64)
                        .build()])
                    .build(),
            )
            .build();
        assert!(!matches_commitment(source, &altered, Some(&deps)));
    }

    async fn mock_chain() -> (DevChain, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let chain = DevChain::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let request = loop {
                    let mut bytes = [0; 4096];
                    let size = stream.read(&mut bytes).await.unwrap();
                    if size == 0 {
                        return;
                    }
                    data.extend_from_slice(&bytes[..size]);
                    if let Some(offset) = data.windows(4).position(|b| b == b"\r\n\r\n") {
                        if let Ok(value) =
                            serde_json::from_slice::<serde_json::Value>(&data[offset + 4..])
                        {
                            break value;
                        }
                    }
                };
                let result = match request["method"].as_str().unwrap() {
                    "get_live_cell" => json!({"status":"dead", "cell":null}),
                    "get_block_median_time" => json!("0x2710"),
                    "get_tip_header" => {
                        let header = ckb_types::core::HeaderBuilder::default()
                            .number(10u64)
                            .epoch(EpochNumberWithFraction::new(2, 1, 2).full_value())
                            .build();
                        serde_json::to_value(ckb_jsonrpc_types::HeaderView::from(header)).unwrap()
                    }
                    _ => serde_json::Value::Null,
                };
                let body = json!({"jsonrpc":"2.0", "id":1, "result":result}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        (chain, server)
    }

    #[tokio::test]
    async fn independent_rpc_rejects_spent_and_immature_inputs() {
        let (chain, server) = mock_chain().await;
        assert!(chain.live_cell(&OutPoint::default()).await.is_err());
        assert!(chain
            .verify_commitment_lineage(
                Hash256::from([1; 32]),
                &OutPoint::new_builder().index(1u32).build()
            )
            .await
            .is_err());
        let input = |since| vec![CellInput::new_builder().since(since).build()];
        assert!(chain.verify_maturity(&input(11u64)).await.is_err());
        assert!(chain.verify_maturity(&input(10u64)).await.is_ok());
        assert!(chain
            .verify_maturity(&input(0x4000_0000_0000_000bu64))
            .await
            .is_err());
        assert!(chain
            .verify_maturity(&input(0x4000_0000_0000_000au64))
            .await
            .is_ok());
        assert!(chain
            .verify_maturity(&input(
                0x2000_0000_0000_0000u64 | EpochNumberWithFraction::new(3, 0, 1).full_value()
            ))
            .await
            .is_err());
        assert!(chain
            .verify_maturity(&input(0x0100_0000_0000_0000u64))
            .await
            .is_err());
        server.abort();
    }
}
