use std::collections::HashMap;

use anyhow::anyhow;
use async_trait::async_trait;
use ckb_jsonrpc_types::Transaction;
use ckb_sdk::{
    CkbRpcClient, ScriptId,
    constants::SIGHASH_TYPE_HASH,
    traits::{
        CellCollector, CellDepResolver, DefaultCellCollector, DefaultCellDepResolver,
        DefaultHeaderDepResolver, DefaultTransactionDependencyProvider, HeaderDepResolver,
        SecpCkbRawKeySigner, TransactionDependencyProvider,
    },
    tx_builder::{CapacityBalancer, TxBuilder, TxBuilderError},
    unlock::{ScriptUnlocker, SecpSighashUnlocker},
};
use ckb_types::{
    core::{BlockView, Capacity, TransactionView},
    packed,
    prelude::*,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{DeserializeAs, SerializeAs, serde_as};

pub struct EntityHex;

impl<T> SerializeAs<T> for EntityHex
where
    T: Entity,
{
    fn serialize_as<S>(source: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("0x{}", &hex::encode(source.as_slice())))
    }
}

impl<'de, T> DeserializeAs<'de, T> for EntityHex
where
    T: Entity,
{
    fn deserialize_as<D>(deserializer: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let v: Vec<u8> = String::deserialize(deserializer).and_then(|string| {
            if string.len() < 2 || &string[..2].to_lowercase() != "0x" {
                return Err(Error::custom(format!(
                    "hex string does not start with 0x: {}",
                    &string
                )));
            };
            hex::decode(&string[2..]).map_err(|err| {
                Error::custom(format!(
                    "failed to decode hex string {}: {:?}",
                    &string, err
                ))
            })
        })?;
        T::from_slice(&v).map_err(Error::custom)
    }
}

#[serde_as]
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FundingRequest {
    /// The funding cell lock script args
    #[serde_as(as = "EntityHex")]
    pub script: packed::Script,
    #[serde_as(as = "Option<EntityHex>")]
    pub udt_type_script: Option<packed::Script>,
    /// Assets amount to be provided by the local party
    pub local_amount: u128,
    /// Fee to be provided by the local party
    pub funding_fee_rate: u64,
    /// Assets amount to be provided by the remote party
    pub remote_amount: u128,
    /// CKB amount to be provided by the local party.
    pub local_reserved_ckb_amount: u64,
    /// CKB amount to be provided by the remote party.
    pub remote_reserved_ckb_amount: u64,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FundingTxBuilder {
    tx: Transaction,
    request: FundingRequest,
    rpc_url: String,
    #[serde_as(as = "EntityHex")]
    funding_source_lock_script: packed::Script,
}

// A copy of fiber funding tx builder that only supports CKB.
// It's assumed that the channel is created by test node 3 to node 1.
#[async_trait]
impl TxBuilder for FundingTxBuilder {
    async fn build_base_async(
        &self,
        _cell_collector: &mut dyn CellCollector,
        _cell_dep_resolver: &dyn CellDepResolver,
        _header_dep_resolver: &dyn HeaderDepResolver,
        _tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, TxBuilderError> {
        let tx: TransactionView = Into::<packed::Transaction>::into(self.tx.clone()).into_view();
        let (funding_cell_output, funding_cell_output_data) = self.build_funding_cell()?;

        // Funding cell does not need new cell deps and header deps. The type script deps will be added with inputs.
        let mut outputs: Vec<packed::CellOutput> = vec![funding_cell_output];
        let mut outputs_data: Vec<packed::Bytes> = vec![funding_cell_output_data];

        for (i, output) in tx.outputs().into_iter().enumerate().skip(1) {
            outputs.push(output.clone());
            outputs_data.push(tx.outputs_data().get(i).unwrap_or_default().clone());
        }

        let builder = tx.as_advanced_builder();

        // set a placeholder_witness for calculating transaction fee according to transaction size
        let placeholder_witness = packed::WitnessArgs::new_builder()
            .lock(Some(molecule::bytes::Bytes::from(vec![0u8; 170])).pack())
            .build();

        let tx_builder = builder
            .set_outputs(outputs)
            .set_outputs_data(outputs_data)
            .set_witnesses(vec![placeholder_witness.as_bytes().pack()]);
        let tx = tx_builder.build();
        Ok(tx)
    }
}

impl FundingTxBuilder {
    #[allow(dead_code)]
    fn build_funding_cell(&self) -> Result<(packed::CellOutput, packed::Bytes), TxBuilderError> {
        // If outputs is not empty, assume that the remote party has already funded.
        let remote_funded = !self.tx.outputs.is_empty();

        let mut ckb_amount = (self.request.local_amount as u64)
            .checked_add(self.request.local_reserved_ckb_amount)
            .ok_or(TxBuilderError::Other(anyhow!("overflow")))?;
        if remote_funded {
            ckb_amount = ckb_amount
                .checked_add(
                    self.request.remote_amount as u64 + self.request.remote_reserved_ckb_amount,
                )
                .ok_or(TxBuilderError::Other(anyhow!("overflow")))?;
        }
        let ckb_output = packed::CellOutput::new_builder()
            .capacity(Capacity::shannons(ckb_amount).pack())
            .lock(self.request.script.clone())
            .build();
        Ok((ckb_output, packed::Bytes::default()))
    }

    #[allow(dead_code)]
    fn build(self) -> Result<Transaction, TxBuilderError> {
        // Build ScriptUnlocker
        let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![]);
        let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
        let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
        let mut unlockers = HashMap::default();
        unlockers.insert(
            sighash_script_id,
            Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
        );

        let sender = self.funding_source_lock_script.clone();

        // Build CapacityBalancer
        let placeholder_witness = packed::WitnessArgs::new_builder()
            .lock(Some(molecule::bytes::Bytes::from(vec![0u8; 170])).pack())
            .build();

        let balancer = CapacityBalancer::new_simple(
            sender.clone(),
            placeholder_witness,
            self.request.funding_fee_rate,
        );

        let ckb_client = CkbRpcClient::new(&self.rpc_url);
        let cell_dep_resolver = {
            match ckb_client
                .get_block_by_number(0.into())
                .map_err(|err| TxBuilderError::Other(err.into()))?
            {
                Some(genesis_block) => {
                    match DefaultCellDepResolver::from_genesis(&BlockView::from(genesis_block)).ok()
                    {
                        Some(ret) => ret,
                        None => {
                            return Err(TxBuilderError::ResolveCellDepFailed(sender));
                        }
                    }
                }
                None => return Err(TxBuilderError::ResolveCellDepFailed(sender)),
            }
        };

        let header_dep_resolver = DefaultHeaderDepResolver::new(&self.rpc_url);
        let mut cell_collector = DefaultCellCollector::new(&self.rpc_url);
        // Exclude existing inputs in the tx
        for existing_input in &self.tx.inputs {
            let used_cell: packed::OutPoint = existing_input.previous_output.clone().into();
            cell_collector.lock_cell(used_cell, u64::MAX)?;
        }
        let tx_dep_provider = DefaultTransactionDependencyProvider::new(&self.rpc_url, 10);

        let (tx, _) = self.build_unlocked(
            &mut cell_collector,
            &cell_dep_resolver,
            &header_dep_resolver,
            &tx_dep_provider,
            &balancer,
            &unlockers,
        )?;

        Ok(tx.data().into())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = std::io::stdin();
    let stdin_lock = stdin.lock();

    // Create a deserializer from stdin
    let mut deserializer = serde_json::Deserializer::from_reader(stdin_lock);

    // Deserialize exactly one JSON value of type serde_json::Value
    let builder = FundingTxBuilder::deserialize(&mut deserializer)?;
    let result = apply_case(builder.clone());

    // Set environment variable FUNDING_TX_SHELL_BUILDER_LOG_FILE when starting fiber nodes to
    // enable logging for funding tx shell builder.
    if let Ok(log_file_path) = std::env::var("FUNDING_TX_SHELL_BUILDER_LOG_FILE") {
        use std::io::Write as _;

        let mut log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file_path)?;

        // Write the JSON value pretty-printed and a newline to the log file
        writeln!(
            log_file,
            "input={}",
            serde_json::to_string_pretty(&builder)?
        )?;
        match &result {
            Ok(tx) => {
                writeln!(log_file, "ok={}", serde_json::to_string_pretty(&tx)?)?;
            }
            Err(err) => {
                writeln!(log_file, "err={}", err)?;
            }
        }
    }

    match result {
        Ok(tx) => {
            println!("{}", serde_json::to_string(&tx)?);
            Ok(())
        }
        Err(err) => {
            eprintln!("{}", err);
            Err(err.into())
        }
    }
}

fn apply_case(builder: FundingTxBuilder) -> Result<Transaction, TxBuilderError> {
    for arg in std::env::args() {
        if arg.starts_with("FUNDING_TX_VERIFICATION_CASE=") {
            let testcase = arg
                .split('=')
                .last()
                .map(|s| s.to_string())
                .unwrap_or_default();
            return apply_case_by_name(builder, testcase);
        }
    }

    Err(TxBuilderError::Other(anyhow!(
        "invalid FUNDING_TX_VERIFICATION_CASE"
    )))
}

fn apply_case_by_name(
    builder: FundingTxBuilder,
    testcase: String,
) -> Result<Transaction, TxBuilderError> {
    match testcase.as_ref() {
        "remove_change" => testcase_remove_change(builder),
        "modify_change" => testcase_modify_change(builder),
        "fund_from_peer" => testcase_fund_from_peer(builder),
        _ => Err(TxBuilderError::Other(anyhow!(
            "invalid FUNDING_TX_VERIFICATION_CASE"
        ))),
    }
}

fn testcase_remove_change(builder: FundingTxBuilder) -> Result<Transaction, TxBuilderError> {
    let mut tx = builder.build()?;
    // Remove the change output for the first funder.
    if tx.outputs.len() == 3 {
        tx.outputs.remove(1);
        tx.outputs_data.remove(1);
    }
    Ok(tx)
}

fn testcase_modify_change(builder: FundingTxBuilder) -> Result<Transaction, TxBuilderError> {
    let mut tx = builder.build()?;
    // Modify the change output for the first funder.
    if tx.outputs.len() == 3 {
        let capacity = tx.outputs[1].capacity.value();
        tx.outputs[1].capacity = (capacity - 1).into();
    }
    Ok(tx)
}

fn testcase_fund_from_peer(mut builder: FundingTxBuilder) -> Result<Transaction, TxBuilderError> {
    if builder.tx.inputs.len() == 1 {
        // Use cells from the peer to fund
        let peer_lock_script: packed::Script = builder.tx.outputs[1].lock.clone().into();
        builder.funding_source_lock_script = peer_lock_script;
    }

    builder.build()
}
