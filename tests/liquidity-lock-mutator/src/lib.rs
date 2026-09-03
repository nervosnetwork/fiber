//! Build and sign CKB transactions that commit mutated liquidity-lock cells.
//!
//! The Bruno liquidity suites use this helper to exercise the rejection paths
//! of `provider_accept_loop_in`: the helper funds a real liquidity-lock output
//! from a dev-chain wallet with exactly one mutated field (lock argument,
//! type script, or UDT cell data), signs the transaction with the wallet's
//! secp256k1 key, and returns the signed transaction together with the
//! outpoint of the mutated cell. The suite then submits the transaction
//! through the CKB JSON-RPC, mines it, and passes the outpoint to
//! `provider_accept_loop_in`, which must reject the cell.
//!
//! The lock argument layout is the 152-byte `build_liquidity_lock_args`
//! layout documented in `tests/bruno/scripts/liquidity-lock-args.js`:
//! payment hash (32), blake2b(claimant lock) (32), blake2b(refund lock) (32),
//! refund since (8, u64 LE), amount (16, u128 LE), asset type hash (32,
//! blake2b of the UDT type script molecule).

use std::collections::HashSet;

use anyhow::{anyhow, bail, Context, Result};
use ckb_hash::blake2b_256;
use ckb_jsonrpc_types::Script as JsonScript;
use ckb_sdk::{
    constants::SIGHASH_TYPE_HASH,
    rpc::ckb_indexer::SearchMode,
    traits::{
        CellCollector, CellDepResolver, CellQueryOptions, DefaultCellCollector,
        DefaultCellDepResolver, DefaultHeaderDepResolver, DefaultTransactionDependencyProvider,
        HeaderDepResolver, SecpCkbRawKeySigner, TransactionDependencyProvider, ValueRangeOption,
    },
    tx_builder::{CapacityBalancer, TxBuilder, TxBuilderError},
    unlock::{ScriptUnlocker, SecpSighashUnlocker},
    util::blake160,
    CkbRpcClient, ScriptId,
};
use ckb_types::{
    bytes::Bytes,
    core::{BlockView, Capacity, DepType, ScriptHashType, TransactionView},
    packed::{self, CellInput, CellOutput, Script},
    prelude::*,
    H256,
};
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Fee rate (shannons per kilo-weight) used by the liquidity runtime for its
/// own lock funding transactions (`DEFAULT_LIQUIDITY_PAYOUT_FEE_RATE`).
pub const LIQUIDITY_FUNDING_FEE_RATE: u64 = 1000;

/// Length of the liquidity-lock script args in bytes.
pub const LOCK_ARGS_LENGTH: usize = 152;

/// Length of a UDT amount encoded as a little-endian u128 in cell data.
pub const UDT_DATA_LENGTH: usize = 16;

/// Genesis output index of the simple UDT contract on the dev chain (see
/// `tests/deploy/udt-init`).
const SIMPLE_UDT_GENESIS_INDEX: u32 = 8;

/// The single field that a built transaction mutates relative to a valid
/// liquidity-lock cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Mutation {
    /// Commit a perfectly valid lock cell (positive control).
    None,
    /// Lock args payment hash does not match the quote.
    PaymentHash,
    /// Lock args blake2b(claimant lock) does not match the quote.
    ClaimantLockHash,
    /// Lock args blake2b(refund lock) does not match the quote.
    RefundLockHash,
    /// Lock args amount field does not match the gross quote amount.
    ArgsAmount,
    /// Lock args refund since does not match the quote.
    RefundSince,
    /// Lock args asset type hash does not match the UDT type script hash.
    AssetTypeHash,
    /// The cell type script differs from the UDT type script of the quote.
    TypeScript,
    /// The UDT cell data length is not 16 bytes.
    DataLength {
        /// The mutated cell data length in bytes (0, 15, or 17).
        length: u64,
    },
    /// The UDT cell data amount differs from the gross quote amount.
    UdtAmount,
}

/// One mutation request for a signed liquidity-lock funding transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutatorRequest {
    /// CKB JSON-RPC endpoint (dev chain with indexer enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    /// Funded dev-chain wallet secret key as hex (node `plain_key` format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privkey: Option<String>,
    /// Payment hash from the imported quote.
    pub payment_hash: String,
    /// Packed molecule bytes of the quote claimant lock.
    pub claimant_lock: String,
    /// Packed molecule bytes of the quote refund lock.
    pub refund_lock: String,
    /// Quote refund since timestamp (`0x` hex u64 with CKB since flag bits).
    pub refund_after_lock_time: String,
    /// Gross on-chain UDT amount of the quote (`0x` hex u128).
    pub gross_amount: String,
    /// Operational CKB capacity of the lock cell (`0x` hex u64).
    pub capacity_ckb: String,
    /// Code hash of the deployed liquidity-lock contract.
    pub liquidity_lock_code_hash: String,
    /// Hash type of the deployed liquidity-lock contract.
    pub liquidity_lock_hash_type: String,
    /// UDT type script registered in the provider asset registry.
    pub udt_type_script: JsonScript,
    /// The field to mutate.
    pub mutation: Mutation,
}

/// Outpoint of the mutated (or valid) lock cell inside the signed transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutatorOutpoint {
    /// Transaction hash of the signed funding transaction.
    pub tx_hash: H256,
    /// Output index of the lock cell, always `"0x0"`.
    pub index: String,
}

/// Successful helper response consumed by the Bruno suite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutatorResponse {
    /// Signed transaction as a CKB JSON-RPC `Transaction` (sendable via
    /// `send_transaction`).
    pub tx: ckb_jsonrpc_types::Transaction,
    /// Transaction hash.
    pub tx_hash: H256,
    /// Outpoint of the lock cell (output index 0).
    pub outpoint: MutatorOutpoint,
}

/// The quote-derived base parameters of a valid liquidity-lock cell.
#[derive(Debug, Clone)]
pub struct BaseLockParams {
    /// Payment hash from the quote.
    pub payment_hash: [u8; 32],
    /// Claimant lock script (packed molecule bytes).
    pub claimant_lock: Script,
    /// Refund lock script (packed molecule bytes).
    pub refund_lock: Script,
    /// Refund since timestamp with CKB since flag bits.
    pub refund_after_lock_time: u64,
    /// Gross on-chain amount carried by the cell.
    pub gross_amount: u128,
    /// Operational CKB capacity of the lock cell.
    pub capacity_ckb: u64,
    /// Liquidity-lock contract code hash.
    pub liquidity_lock_code_hash: [u8; 32],
    /// Liquidity-lock contract hash type byte.
    pub liquidity_lock_hash_type: u8,
    /// UDT type script of the quote asset.
    pub udt_type_script: Script,
}

/// The mutated lock cell specification produced by [`apply_mutation`].
#[derive(Debug, Clone)]
pub struct MutatedCellSpec {
    /// The 152-byte lock script args (mutated or base).
    pub args: [u8; LOCK_ARGS_LENGTH],
    /// The cell type script (mutated for [`Mutation::TypeScript`]).
    pub type_script: Script,
    /// The cell data bytes (mutated length or amount).
    pub data: Vec<u8>,
    /// The u128 amount the simple UDT script conserves for the lock cell,
    /// derived from the mutated data with the same zero-padding rule the
    /// contract uses.
    pub conservation_amount: u128,
}

fn parse_hex_bytes(value: &str, field: &str) -> Result<Vec<u8>> {
    let trimmed = value.trim();
    let hex_part = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    hex::decode(hex_part).map_err(|error| anyhow!("{field} is not valid hex: {error}"))
}

fn parse_fixed_hex(value: &str, expected: usize, field: &str) -> Result<[u8; 32]> {
    let bytes = parse_hex_bytes(value, field)?;
    if bytes.len() != expected {
        bail!("{field} must be {expected} bytes, got {}", bytes.len());
    }
    let mut out = [0u8; 32];
    out[..expected].copy_from_slice(&bytes[..expected]);
    Ok(out)
}

fn parse_u64_hex(value: &str, field: &str) -> Result<u64> {
    let trimmed = value.trim();
    let parsed = if let Some(hex_part) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex_part, 16)
    } else {
        trimmed.parse::<u64>()
    };
    parsed.map_err(|error| anyhow!("{field} is not a valid u64: {error}"))
}

fn parse_u128_hex(value: &str, field: &str) -> Result<u128> {
    let trimmed = value.trim();
    let parsed = if let Some(hex_part) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u128::from_str_radix(hex_part, 16)
    } else {
        trimmed.parse::<u128>()
    };
    parsed.map_err(|error| anyhow!("{field} is not a valid u128: {error}"))
}

fn parse_hash_type_byte(value: &str, field: &str) -> Result<u8> {
    match value.trim() {
        "data" => Ok(0),
        "type" => Ok(1),
        "data1" => Ok(2),
        "data2" => Ok(3),
        other => bail!("{field} must be one of data|type|data1|data2, got {other}"),
    }
}

/// Parse a wallet secret key from its hex representation.
pub fn parse_secret_key(value: &str) -> Result<SecretKey> {
    let bytes = parse_hex_bytes(value, "privkey")?;
    if bytes.len() != 32 {
        bail!("privkey must be 32 bytes, got {}", bytes.len());
    }
    SecretKey::from_slice(&bytes).map_err(|error| anyhow!("invalid privkey: {error}"))
}

/// Parse a wallet secret key from a node `plain_key` file (hex, 32 bytes).
pub fn parse_secret_key_file(path: &str) -> Result<SecretKey> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read privkey file {path}"))?;
    parse_secret_key(&content)
}

/// The secp256k1-blake160-sighash-all lock script of the funding wallet.
pub fn sender_lock_script(secret_key: &SecretKey) -> Script {
    let secp256k1 = secp256k1::SECP256K1;
    let pubkey = secret_key.public_key(secp256k1).serialize();
    Script::new_builder()
        .code_hash(SIGHASH_TYPE_HASH.clone().pack())
        .hash_type::<packed::Byte>(ScriptHashType::Type.into())
        .args(Bytes::from(blake160(&pubkey).as_bytes().to_vec()).pack())
        .build()
}

/// Build the 152-byte liquidity-lock args for the base quote parameters.
pub fn build_lock_args(base: &BaseLockParams) -> [u8; LOCK_ARGS_LENGTH] {
    let mut args = [0u8; LOCK_ARGS_LENGTH];
    args[0..32].copy_from_slice(&base.payment_hash);
    args[32..64].copy_from_slice(&blake2b_256(base.claimant_lock.as_slice()));
    args[64..96].copy_from_slice(&blake2b_256(base.refund_lock.as_slice()));
    args[96..104].copy_from_slice(&base.refund_after_lock_time.to_le_bytes());
    args[104..120].copy_from_slice(&base.gross_amount.to_le_bytes());
    args[120..152].copy_from_slice(&blake2b_256(base.udt_type_script.as_slice()));
    args
}

fn mutated_type_script(udt_type_script: &Script) -> Script {
    udt_type_script
        .clone()
        .as_builder()
        .args(Bytes::from(vec![0xEE; 32]).pack())
        .build()
}

/// Apply one mutation to the base lock cell, producing the exact cell
/// specification the signed transaction commits on chain.
pub fn apply_mutation(base: &BaseLockParams, mutation: &Mutation) -> Result<MutatedCellSpec> {
    let args = build_lock_args(base);
    let gross_le = base.gross_amount.to_le_bytes();
    let valid_cell = MutatedCellSpec {
        args,
        type_script: base.udt_type_script.clone(),
        data: gross_le.to_vec(),
        conservation_amount: base.gross_amount,
    };
    match mutation {
        Mutation::None => Ok(valid_cell),
        Mutation::PaymentHash => Ok(MutatedCellSpec {
            args: flip_arg_byte(args, 0),
            ..valid_cell
        }),
        Mutation::ClaimantLockHash => Ok(MutatedCellSpec {
            args: flip_arg_byte(args, 32),
            ..valid_cell
        }),
        Mutation::RefundLockHash => Ok(MutatedCellSpec {
            args: flip_arg_byte(args, 64),
            ..valid_cell
        }),
        Mutation::ArgsAmount => {
            let mutated_amount = base
                .gross_amount
                .checked_add(1)
                .ok_or_else(|| anyhow!("gross amount overflow mutating args amount"))?;
            let mut args = args;
            args[104..120].copy_from_slice(&mutated_amount.to_le_bytes());
            Ok(MutatedCellSpec { args, ..valid_cell })
        }
        Mutation::RefundSince => {
            let mutated_since = base
                .refund_after_lock_time
                .checked_add(1)
                .ok_or_else(|| anyhow!("refund since overflow mutating refund since"))?;
            let mut args = args;
            args[96..104].copy_from_slice(&mutated_since.to_le_bytes());
            Ok(MutatedCellSpec { args, ..valid_cell })
        }
        Mutation::AssetTypeHash => Ok(MutatedCellSpec {
            args: flip_arg_byte(args, 120),
            ..valid_cell
        }),
        Mutation::TypeScript => {
            // The mutated type script forms its own (empty) UDT conservation
            // group, so the lock cell must carry empty data, while the change
            // output conserves the full gross amount under the real type
            // script. The provider rejects the cell on the type script
            // comparison before looking at the data.
            Ok(MutatedCellSpec {
                type_script: mutated_type_script(&base.udt_type_script),
                data: Vec::new(),
                conservation_amount: 0,
                ..valid_cell
            })
        }
        Mutation::DataLength { length } => match length {
            0 => Ok(MutatedCellSpec {
                data: Vec::new(),
                conservation_amount: 0,
                ..valid_cell
            }),
            15 => {
                if base.gross_amount >= (1u128 << 120) {
                    bail!("data length 15 mutation requires a gross amount below 2^120");
                }
                Ok(MutatedCellSpec {
                    data: gross_le[..15].to_vec(),
                    conservation_amount: base.gross_amount,
                    ..valid_cell
                })
            }
            17 => Ok(MutatedCellSpec {
                data: [gross_le.as_slice(), &[0x00]].concat(),
                conservation_amount: base.gross_amount,
                ..valid_cell
            }),
            other => bail!("unsupported data length mutation: {other}"),
        },
        Mutation::UdtAmount => {
            let mutated_amount = base
                .gross_amount
                .checked_add(1)
                .ok_or_else(|| anyhow!("gross amount overflow mutating udt amount"))?;
            Ok(MutatedCellSpec {
                data: mutated_amount.to_le_bytes().to_vec(),
                conservation_amount: mutated_amount,
                ..valid_cell
            })
        }
    }
}

fn flip_arg_byte(mut args: [u8; LOCK_ARGS_LENGTH], offset: usize) -> [u8; LOCK_ARGS_LENGTH] {
    args[offset] ^= 0xff;
    args
}

fn hash_type_from_byte(byte: u8) -> ScriptHashType {
    match byte {
        0 => ScriptHashType::Data,
        1 => ScriptHashType::Type,
        2 => ScriptHashType::Data1,
        3 => ScriptHashType::Data2,
        other => panic!("invalid hash type byte: {other}"),
    }
}

fn build_liquidity_lock_script(base: &BaseLockParams, args: &[u8; LOCK_ARGS_LENGTH]) -> Script {
    Script::new_builder()
        .code_hash(H256::from(base.liquidity_lock_code_hash).pack())
        .hash_type::<packed::Byte>(hash_type_from_byte(base.liquidity_lock_hash_type).into())
        .args(Bytes::from(args.to_vec()).pack())
        .build()
}

/// A live UDT input cell selected by the cell collector.
#[derive(Debug, Clone)]
pub struct UdtInput {
    /// Outpoint of the live UDT cell.
    pub outpoint: packed::OutPoint,
    /// UDT amount carried by the cell data.
    pub amount: u128,
    /// CKB capacity of the cell.
    pub capacity: u64,
}

/// Assemble the unsigned funding transaction that commits the mutated lock
/// cell. The lock cell is output 0; a UDT change output conserves the mutated
/// UDT group; CKB inputs and the CKB change output are added by the ckb-sdk
/// capacity balancer before signing.
pub fn assemble_lock_transaction(
    base: &BaseLockParams,
    mutated: &MutatedCellSpec,
    sender_lock: &Script,
    udt_inputs: &[UdtInput],
    cell_deps: Vec<packed::CellDep>,
) -> Result<TransactionView> {
    let input_amount: u128 = udt_inputs.iter().try_fold(0u128, |acc, input| {
        acc.checked_add(input.amount)
            .ok_or_else(|| anyhow!("udt input amount overflow"))
    })?;
    let change_amount = input_amount
        .checked_sub(mutated.conservation_amount)
        .ok_or_else(|| {
            anyhow!(
                "udt inputs hold {input_amount} atoms but the mutated cell must conserve {}",
                mutated.conservation_amount
            )
        })?;

    let mut outputs = Vec::new();
    let mut outputs_data = Vec::new();

    let lock_output = CellOutput::new_builder()
        .capacity(Capacity::shannons(base.capacity_ckb).pack())
        .lock(build_liquidity_lock_script(base, &mutated.args))
        .type_(Some(mutated.type_script.clone()).pack())
        .build();
    outputs.push(lock_output);
    outputs_data.push(Bytes::from(mutated.data.clone()).pack());

    if change_amount > 0 {
        let change_data = Bytes::from(change_amount.to_le_bytes().to_vec());
        let change_output_data_len = change_data.len();
        let change_output = CellOutput::new_builder()
            .lock(sender_lock.clone())
            .type_(Some(base.udt_type_script.clone()).pack())
            .build();
        let change_capacity = change_output
            .occupied_capacity(Capacity::bytes(change_output_data_len)?)
            .map_err(|error| anyhow!("change occupied capacity: {error}"))?;
        outputs.push(
            change_output
                .as_builder()
                .capacity(change_capacity.pack())
                .build(),
        );
        outputs_data.push(change_data.pack());
    }

    let inputs: Vec<CellInput> = udt_inputs
        .iter()
        .map(|input| CellInput::new(input.outpoint.clone(), 0))
        .collect();
    if inputs.is_empty() {
        bail!("no UDT input cells collected");
    }

    // A placeholder signature group keeps the secp256k1 witness layout in
    // place for the capacity balancer and the signer.
    let placeholder_witness = packed::WitnessArgs::new_builder()
        .lock(Some(Bytes::from(vec![0u8; 65])).pack())
        .build();

    let tx = packed::Transaction::default()
        .as_advanced_builder()
        .cell_deps(cell_deps)
        .inputs(inputs)
        .outputs(outputs)
        .outputs_data(outputs_data)
        .witness(placeholder_witness.as_bytes().pack())
        .build();
    Ok(tx)
}

/// Derive the cell deps from the dev-chain genesis block: the secp256k1
/// dep group (genesis transaction 1, output 0) and the simple UDT code cell
/// (genesis transaction 0, output 8, see `tests/deploy/udt-init`).
pub fn cell_deps_from_genesis(
    genesis: &BlockView,
    udt_type_script: &Script,
) -> Result<Vec<packed::CellDep>> {
    let genesis_tx0 = genesis
        .transaction(0)
        .context("genesis transaction 0 is missing")?;
    let genesis_tx1 = genesis
        .transaction(1)
        .context("genesis transaction 1 is missing")?;

    let udt_output = genesis_tx0
        .outputs()
        .get(SIMPLE_UDT_GENESIS_INDEX as usize)
        .context("genesis output for the simple UDT contract is missing")?
        .clone();
    let udt_output_data = genesis_tx0
        .outputs_data()
        .get(SIMPLE_UDT_GENESIS_INDEX as usize)
        .context("genesis output data for the simple UDT contract is missing")?
        .raw_data();
    let udt_code_hash: H256 = CellOutput::calc_data_hash(&udt_output_data).unpack();
    let declared_code_hash: H256 = udt_output
        .type_()
        .to_opt()
        .context("simple UDT genesis output has no type script")?
        .code_hash()
        .unpack();
    if declared_code_hash != udt_code_hash {
        bail!("genesis simple UDT output has an inconsistent type script");
    }
    if udt_code_hash != udt_type_script.code_hash().unpack() {
        bail!(
            "udt_type_script code hash does not match the dev-chain simple UDT contract at genesis index {SIMPLE_UDT_GENESIS_INDEX}"
        );
    }

    let secp_dep = packed::CellDep::new_builder()
        .out_point(
            packed::OutPoint::new_builder()
                .tx_hash(genesis_tx1.hash())
                .index(packed::Uint32::default())
                .build(),
        )
        .dep_type(DepType::DepGroup)
        .build();
    let udt_dep = packed::CellDep::new_builder()
        .out_point(
            packed::OutPoint::new_builder()
                .tx_hash(genesis_tx0.hash())
                .index::<packed::Uint32>(SIMPLE_UDT_GENESIS_INDEX.into())
                .build(),
        )
        .dep_type(DepType::Code)
        .build();
    Ok(vec![secp_dep, udt_dep])
}

/// Collect live UDT cells of the sender covering the target amount.
pub fn collect_udt_inputs(
    cell_collector: &mut DefaultCellCollector,
    sender_lock: &Script,
    udt_type_script: &Script,
    target_amount: u128,
) -> Result<Vec<UdtInput>> {
    let mut query = CellQueryOptions::new_lock(sender_lock.clone());
    query.script_search_mode = Some(SearchMode::Exact);
    query.secondary_script = Some(udt_type_script.clone());
    query.data_len_range = Some(ValueRangeOption::new_min(UDT_DATA_LENGTH as u64));

    let (cells, _) = cell_collector
        .collect_live_cells(&query, true)
        .map_err(|error| anyhow!("collect live UDT cells: {error}"))?;
    let mut inputs = Vec::new();
    let mut found: u128 = 0;
    for cell in cells {
        if cell.output_data.len() < UDT_DATA_LENGTH {
            continue;
        }
        let mut amount_bytes = [0u8; 16];
        amount_bytes.copy_from_slice(&cell.output_data.as_ref()[..UDT_DATA_LENGTH]);
        let amount = u128::from_le_bytes(amount_bytes);
        found = found
            .checked_add(amount)
            .ok_or_else(|| anyhow!("udt input amount overflow"))?;
        inputs.push(UdtInput {
            outpoint: cell.out_point,
            amount,
            capacity: cell.output.capacity().unpack(),
        });
        if found >= target_amount {
            return Ok(inputs);
        }
    }
    bail!(
        "insufficient UDT cells: found {found} atoms, need {target_amount} atoms for the mutated lock cell"
    )
}

/// The ckb-sdk transaction builder wrapper that keeps the pre-assembled
/// mutated lock transaction as its base and lets the capacity balancer fund
/// the CKB side before the raw-key signer unlocks the secp group.
struct FundedTxBuilder {
    tx: TransactionView,
}

#[async_trait::async_trait]
impl TxBuilder for FundedTxBuilder {
    async fn build_base_async(
        &self,
        _cell_collector: &mut dyn CellCollector,
        _cell_dep_resolver: &dyn CellDepResolver,
        _header_dep_resolver: &dyn HeaderDepResolver,
        _tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> std::result::Result<TransactionView, TxBuilderError> {
        Ok(self.tx.clone())
    }
}

/// Balance the assembled transaction with CKB capacity from the sender wallet
/// and sign its secp256k1 group, mirroring the runtime local-wallet funding
/// flow (`LocalSigner::sign_funding_tx`).
#[allow(clippy::too_many_arguments)]
pub fn fund_and_sign_transaction(
    tx: TransactionView,
    sender_lock: &Script,
    secret_key: &SecretKey,
    cell_dep_resolver: &DefaultCellDepResolver,
    header_dep_resolver: &DefaultHeaderDepResolver,
    tx_dep_provider: &DefaultTransactionDependencyProvider,
    cell_collector: &mut DefaultCellCollector,
) -> Result<TransactionView> {
    let signer = SecpCkbRawKeySigner::new_with_secret_keys(vec![*secret_key]);
    let sighash_unlocker = SecpSighashUnlocker::from(Box::new(signer) as Box<_>);
    let sighash_script_id = ScriptId::new_type(SIGHASH_TYPE_HASH.clone());
    let mut unlockers: std::collections::HashMap<ScriptId, Box<dyn ScriptUnlocker>> =
        std::collections::HashMap::default();
    unlockers.insert(
        sighash_script_id,
        Box::new(sighash_unlocker) as Box<dyn ScriptUnlocker>,
    );

    let placeholder_witness = packed::WitnessArgs::new_builder()
        .lock(Some(Bytes::from(vec![0u8; 65])).pack())
        .build();
    let balancer = CapacityBalancer::new_simple(
        sender_lock.clone(),
        placeholder_witness,
        LIQUIDITY_FUNDING_FEE_RATE,
    );

    let builder = FundedTxBuilder { tx };
    let (signed_tx, _unlocked_groups) = builder
        .build_unlocked(
            cell_collector,
            cell_dep_resolver,
            header_dep_resolver,
            tx_dep_provider,
            &balancer,
            &unlockers,
        )
        .map_err(|error| anyhow!("fund and sign mutated lock transaction: {error}"))?;
    Ok(signed_tx)
}

/// Parse a [`MutatorRequest`] into the base lock parameters.
pub fn parse_base_params(request: &MutatorRequest) -> Result<BaseLockParams> {
    let claimant_lock_bytes = parse_hex_bytes(&request.claimant_lock, "claimant_lock")?;
    let claimant_lock = Script::from_slice(&claimant_lock_bytes)
        .map_err(|error| anyhow!("claimant_lock is not a molecule script: {error}"))?
        .clone();
    let refund_lock_bytes = parse_hex_bytes(&request.refund_lock, "refund_lock")?;
    let refund_lock = Script::from_slice(&refund_lock_bytes)
        .map_err(|error| anyhow!("refund_lock is not a molecule script: {error}"))?
        .clone();
    let udt_type_script: Script = request.udt_type_script.clone().into();
    Ok(BaseLockParams {
        payment_hash: parse_fixed_hex(&request.payment_hash, 32, "payment_hash")?,
        claimant_lock,
        refund_lock,
        refund_after_lock_time: parse_u64_hex(
            &request.refund_after_lock_time,
            "refund_after_lock_time",
        )?,
        gross_amount: parse_u128_hex(&request.gross_amount, "gross_amount")?,
        capacity_ckb: parse_u64_hex(&request.capacity_ckb, "capacity_ckb")?,
        liquidity_lock_code_hash: parse_fixed_hex(
            &request.liquidity_lock_code_hash,
            32,
            "liquidity_lock_code_hash",
        )?,
        liquidity_lock_hash_type: parse_hash_type_byte(
            &request.liquidity_lock_hash_type,
            "liquidity_lock_hash_type",
        )?,
        udt_type_script,
    })
}

/// Handle one mutation request end to end: collect cells, build, balance,
/// sign, and return the JSON-RPC-ready response together with every input
/// outpoint consumed by the signed transaction. The caller persists the
/// outpoint set so later requests never reuse already-committed cells.
pub fn handle_request(
    request: &MutatorRequest,
    rpc_url: &str,
    privkey: &SecretKey,
    previously_locked: &HashSet<packed::OutPoint>,
) -> Result<(MutatorResponse, HashSet<packed::OutPoint>)> {
    let base = parse_base_params(request)?;
    let mutated = apply_mutation(&base, &request.mutation)?;
    let sender_lock = sender_lock_script(privkey);

    let ckb_client = CkbRpcClient::new(rpc_url);
    let genesis_block = ckb_client
        .get_block_by_number(0u64.into())
        .map_err(|error| anyhow!("fetch dev-chain genesis block: {error}"))?
        .context("dev chain has no genesis block")?;
    let genesis = BlockView::from(genesis_block);
    let cell_deps = cell_deps_from_genesis(&genesis, &base.udt_type_script)?;
    let cell_dep_resolver = DefaultCellDepResolver::from_genesis(&genesis)
        .context("resolve secp cell deps from genesis")?;

    let mut cell_collector = DefaultCellCollector::new(rpc_url);
    for outpoint in previously_locked {
        cell_collector
            .lock_cell(outpoint.clone(), u64::MAX)
            .map_err(|error| anyhow!("lock previously used input: {error}"))?;
    }

    let target_amount = base.gross_amount.max(mutated.conservation_amount);
    let udt_inputs = collect_udt_inputs(
        &mut cell_collector,
        &sender_lock,
        &base.udt_type_script,
        target_amount,
    )?;
    for input in &udt_inputs {
        cell_collector
            .lock_cell(input.outpoint.clone(), u64::MAX)
            .map_err(|error| anyhow!("lock collected input: {error}"))?;
    }

    let header_dep_resolver = DefaultHeaderDepResolver::new(rpc_url);
    let tx_dep_provider = DefaultTransactionDependencyProvider::new(rpc_url, 10);

    let unsigned =
        assemble_lock_transaction(&base, &mutated, &sender_lock, &udt_inputs, cell_deps)?;
    let signed = fund_and_sign_transaction(
        unsigned,
        &sender_lock,
        privkey,
        &cell_dep_resolver,
        &header_dep_resolver,
        &tx_dep_provider,
        &mut cell_collector,
    )?;

    let mut used_outpoints: HashSet<packed::OutPoint> = udt_inputs
        .iter()
        .map(|input| input.outpoint.clone())
        .collect();
    for outpoint in signed.input_pts_iter() {
        used_outpoints.insert(outpoint);
    }

    let tx_hash = signed.hash();
    Ok((
        MutatorResponse {
            tx: ckb_jsonrpc_types::TransactionView::from(signed).inner,
            tx_hash: tx_hash.unpack(),
            outpoint: MutatorOutpoint {
                tx_hash: tx_hash.unpack(),
                index: "0x0".to_string(),
            },
        },
        used_outpoints,
    ))
}

/// Parse a raw JSON payload into a [`MutatorRequest`].
pub fn parse_request_payload(payload: &str) -> Result<MutatorRequest> {
    let value: JsonValue = serde_json::from_str(payload).context("request is not valid JSON")?;
    serde_json::from_value(value).context("request JSON does not match the mutator schema")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ckb_types::core::{BlockBuilder, TransactionBuilder};

    /// The data hash of the fake genesis UDT contract code used by the
    /// genesis fixture below.
    fn fake_udt_code_hash() -> [u8; 32] {
        let code_hash: H256 = CellOutput::calc_data_hash(&[0xAB_u8; 64]).unpack();
        code_hash.0
    }

    fn base_params() -> BaseLockParams {
        BaseLockParams {
            payment_hash: [0x11; 32],
            claimant_lock: Script::new_builder()
                .code_hash([0x22; 32].pack())
                .hash_type::<packed::Byte>(ScriptHashType::Type.into())
                .args(Bytes::from(vec![0x33; 20]).pack())
                .build(),
            refund_lock: Script::new_builder()
                .code_hash([0x44; 32].pack())
                .hash_type::<packed::Byte>(ScriptHashType::Data.into())
                .args(Bytes::from(vec![0x55; 21]).pack())
                .build(),
            refund_after_lock_time: 0x5100000000012600,
            gross_amount: 1001,
            capacity_ckb: 1000,
            liquidity_lock_code_hash: [0x66; 32],
            liquidity_lock_hash_type: 3,
            udt_type_script: Script::new_builder()
                .code_hash(H256::from(fake_udt_code_hash()).pack())
                .hash_type::<packed::Byte>(ScriptHashType::Data2.into())
                .args(Bytes::from(vec![0x88; 32]).pack())
                .build(),
        }
    }

    fn fake_genesis() -> BlockView {
        // Genesis transaction 0 carries the UDT contract code cell at output
        // 8; genesis transaction 1 is the secp dep group placeholder.
        let mut outputs = Vec::new();
        let mut outputs_data: Vec<packed::Bytes> = Vec::new();
        for _ in 0..SIMPLE_UDT_GENESIS_INDEX {
            outputs.push(
                CellOutput::new_builder()
                    .capacity(Capacity::shannons(1).pack())
                    .build(),
            );
            outputs_data.push(packed::Bytes::default());
        }
        let udt_code: Vec<u8> = vec![0xAB; 64];
        let udt_code_hash: H256 = CellOutput::calc_data_hash(&udt_code).unpack();
        outputs.push(
            CellOutput::new_builder()
                .capacity(Capacity::shannons(1000).pack())
                .type_(
                    Some(
                        Script::new_builder()
                            .code_hash(udt_code_hash.pack())
                            .hash_type::<packed::Byte>(ScriptHashType::Data2.into())
                            .args(Bytes::default().pack())
                            .build(),
                    )
                    .pack(),
                )
                .build(),
        );
        outputs_data.push(packed::Bytes::from(udt_code));
        let genesis_tx0 = TransactionBuilder::default()
            .outputs(outputs)
            .outputs_data(outputs_data)
            .build();
        let genesis_tx1 = TransactionBuilder::default().build();
        BlockBuilder::default()
            .transaction(genesis_tx0)
            .transaction(genesis_tx1)
            .build()
    }

    fn sender() -> Script {
        sender_lock_script(&SecretKey::from_slice(&[9u8; 32]).expect("valid key"))
    }

    fn two_inputs() -> Vec<UdtInput> {
        vec![
            UdtInput {
                outpoint: packed::OutPoint::new_builder()
                    .tx_hash([1u8; 32].pack())
                    .index(packed::Uint32::default())
                    .build(),
                amount: 600,
                capacity: 62_000_000,
            },
            UdtInput {
                outpoint: packed::OutPoint::new_builder()
                    .tx_hash([2u8; 32].pack())
                    .index::<packed::Uint32>(1u32.into())
                    .build(),
                amount: 500,
                capacity: 62_000_000,
            },
        ]
    }

    fn assert_args_field_unchanged(
        mutated: &MutatedCellSpec,
        base_args: &[u8],
        range: std::ops::Range<usize>,
    ) {
        let mutated_slice = &mutated.args[range.clone()];
        let base_slice = &base_args[range.clone()];
        assert_eq!(
            mutated_slice, base_slice,
            "args bytes {range:?} must not change"
        );
    }

    #[test]
    fn lock_args_layout_matches_quote_terms() {
        let base = base_params();
        let args = build_lock_args(&base);
        assert_eq!(args.len(), 152);
        assert_eq!(&args[0..32], &base.payment_hash);
        assert_eq!(
            &args[32..64],
            &blake2b_256(base.claimant_lock.as_slice())[..]
        );
        assert_eq!(&args[64..96], &blake2b_256(base.refund_lock.as_slice())[..]);
        assert_eq!(&args[96..104], &base.refund_after_lock_time.to_le_bytes());
        assert_eq!(&args[104..120], &base.gross_amount.to_le_bytes());
        assert_eq!(
            &args[120..152],
            &blake2b_256(base.udt_type_script.as_slice())[..]
        );
    }

    #[test]
    fn single_byte_mutations_flip_exactly_one_arg_byte() {
        let base = base_params();
        let base_args = build_lock_args(&base);
        let cases = [
            (Mutation::PaymentHash, 0usize),
            (Mutation::ClaimantLockHash, 32usize),
            (Mutation::RefundLockHash, 64usize),
            (Mutation::AssetTypeHash, 120usize),
        ];
        for (mutation, offset) in cases {
            let mutated = apply_mutation(&base, &mutation).expect("mutation applies");
            assert_eq!(mutated.args[offset], base_args[offset] ^ 0xff);
            let mut others = base_args;
            others[offset] = mutated.args[offset];
            assert_eq!(mutated.args, others, "only byte {offset} may change");
            assert_eq!(mutated.data, base.gross_amount.to_le_bytes());
            assert_eq!(mutated.type_script, base.udt_type_script);
            assert_eq!(mutated.conservation_amount, base.gross_amount);
        }
    }

    #[test]
    fn args_amount_and_refund_since_mutations_bump_the_field() {
        let base = base_params();
        let base_args = build_lock_args(&base);

        let amount = apply_mutation(&base, &Mutation::ArgsAmount).expect("applies");
        assert_eq!(
            u128::from_le_bytes(amount.args[104..120].try_into().expect("16 bytes")),
            base.gross_amount + 1
        );
        assert_args_field_unchanged(&amount, &base_args, 0..104);
        assert_args_field_unchanged(&amount, &base_args, 120..152);
        assert_eq!(amount.data, base.gross_amount.to_le_bytes());

        let since = apply_mutation(&base, &Mutation::RefundSince).expect("applies");
        assert_eq!(
            u64::from_le_bytes(since.args[96..104].try_into().expect("8 bytes")),
            base.refund_after_lock_time + 1
        );
        assert_args_field_unchanged(&since, &base_args, 0..96);
        assert_args_field_unchanged(&since, &base_args, 104..152);
        assert_eq!(since.data, base.gross_amount.to_le_bytes());
    }

    #[test]
    fn type_script_mutation_swaps_type_args_and_clears_data() {
        let base = base_params();
        let mutated = apply_mutation(&base, &Mutation::TypeScript).expect("applies");
        assert_eq!(build_lock_args(&base), mutated.args);
        assert_ne!(mutated.type_script, base.udt_type_script);
        assert_eq!(
            mutated.type_script.code_hash(),
            base.udt_type_script.code_hash()
        );
        assert_eq!(
            mutated.type_script.args().raw_data().to_vec(),
            vec![0xEE; 32]
        );
        assert!(mutated.data.is_empty());
        assert_eq!(mutated.conservation_amount, 0);
    }

    #[test]
    fn data_length_mutations_cover_zero_fifteen_and_seventeen() {
        let base = base_params();
        for (length, expected_data_len, expected_conservation) in
            [(0u64, 0usize, 0u128), (15, 15, 1001), (17, 17, 1001)]
        {
            let mutated = apply_mutation(&base, &Mutation::DataLength { length }).expect("applies");
            assert_eq!(mutated.data.len(), expected_data_len);
            assert_eq!(mutated.conservation_amount, expected_conservation);
            assert_eq!(mutated.args, build_lock_args(&base));
            assert_eq!(mutated.type_script, base.udt_type_script);
        }
        for length in [1u64, 14, 16, 18] {
            assert!(apply_mutation(&base, &Mutation::DataLength { length }).is_err());
        }
    }

    #[test]
    fn data_length_fifteen_requires_small_gross_amount() {
        let mut base = base_params();
        base.gross_amount = 1u128 << 120;
        assert!(apply_mutation(&base, &Mutation::DataLength { length: 15 }).is_err());
    }

    #[test]
    fn udt_amount_mutation_keeps_args_amount_correct() {
        let base = base_params();
        let mutated = apply_mutation(&base, &Mutation::UdtAmount).expect("applies");
        assert_eq!(mutated.args, build_lock_args(&base));
        assert_eq!(
            u128::from_le_bytes(mutated.data.clone().try_into().expect("16 bytes")),
            base.gross_amount + 1
        );
        assert_eq!(mutated.conservation_amount, base.gross_amount + 1);
    }

    #[test]
    fn assembled_transaction_commits_the_mutated_cell_and_conserves_udt() {
        let base = base_params();
        let mutated = apply_mutation(&base, &Mutation::None).expect("applies");
        let cell_deps =
            cell_deps_from_genesis(&fake_genesis(), &base.udt_type_script).expect("cell deps");
        let tx =
            assemble_lock_transaction(&base, &mutated, &sender(), &two_inputs(), cell_deps.clone())
                .expect("assembles");

        assert_eq!(tx.inputs().len(), 2);
        assert_eq!(tx.cell_deps().len(), cell_deps.len());
        assert_eq!(tx.witnesses().len(), 1);

        let lock_output = tx.outputs().get(0).expect("lock output").clone();
        assert_eq!(u64::from(lock_output.capacity()), base.capacity_ckb);
        let lock = lock_output.lock();
        assert_eq!(
            lock.code_hash().raw_data().to_vec(),
            base.liquidity_lock_code_hash.to_vec()
        );
        assert_eq!(lock.args().raw_data().to_vec(), mutated.args.to_vec());
        assert_eq!(lock_output.type_().to_opt(), Some(mutated.type_script));
        assert_eq!(
            tx.outputs_data().get(0).expect("data").raw_data().to_vec(),
            base.gross_amount.to_le_bytes().to_vec()
        );

        // Change output conserves 1100 - 1001 = 99 atoms under the real UDT
        // script and is locked by the sender.
        let change = tx.outputs().get(1).expect("change output").clone();
        assert_eq!(change.lock(), sender());
        assert_eq!(change.type_().to_opt(), Some(base.udt_type_script.clone()));
        let change_data = tx.outputs_data().get(1).expect("change data").raw_data();
        assert_eq!(change_data.len(), 16);
        assert_eq!(
            u128::from_le_bytes(change_data.as_ref().try_into().expect("16 bytes")),
            99
        );
        let expected_change = CellOutput::new_builder()
            .lock(sender())
            .type_(Some(base.udt_type_script.clone()).pack())
            .build()
            .occupied_capacity(Capacity::bytes(16).expect("16 bytes is valid"))
            .expect("occupied capacity");
        assert_eq!(u64::from(change.capacity()), expected_change.as_u64());
    }

    #[test]
    fn type_script_mutation_change_output_conserves_full_gross() {
        let base = base_params();
        let mutated = apply_mutation(&base, &Mutation::TypeScript).expect("applies");
        let tx = assemble_lock_transaction(
            &base,
            &mutated,
            &sender(),
            &two_inputs(),
            cell_deps_from_genesis(&fake_genesis(), &base.udt_type_script).expect("deps"),
        )
        .expect("assembles");

        // The lock cell data is empty for the mutated type script group.
        assert_eq!(
            tx.outputs_data()
                .get(0)
                .expect("lock data")
                .raw_data()
                .len(),
            0
        );
        let change = tx.outputs().get(1).expect("change output").clone();
        let change_data = tx.outputs_data().get(1).expect("change data").raw_data();
        assert_eq!(
            u128::from_le_bytes(change_data.as_ref().try_into().expect("16 bytes")),
            1100
        );
        assert_eq!(change.type_().to_opt(), Some(base.udt_type_script));
    }

    #[test]
    fn empty_data_mutation_conserves_via_change_output() {
        let base = base_params();
        let mutated = apply_mutation(&base, &Mutation::DataLength { length: 0 }).expect("applies");
        let tx = assemble_lock_transaction(&base, &mutated, &sender(), &two_inputs(), Vec::new())
            .expect("assembles");
        let change_data = tx.outputs_data().get(1).expect("change data").raw_data();
        assert_eq!(
            u128::from_le_bytes(change_data.as_ref().try_into().expect("16 bytes")),
            1100
        );
    }

    #[test]
    fn assembly_rejects_insufficient_udt_inputs() {
        let base = base_params();
        let mutated = apply_mutation(&base, &Mutation::None).expect("applies");
        let inputs = vec![UdtInput {
            outpoint: packed::OutPoint::new_builder()
                .tx_hash([1u8; 32].pack())
                .index(packed::Uint32::default())
                .build(),
            amount: 500,
            capacity: 62_000_000,
        }];
        let error = assemble_lock_transaction(&base, &mutated, &sender(), &inputs, Vec::new())
            .expect_err("must reject");
        assert!(error.to_string().contains("must conserve"));
    }

    #[test]
    fn cell_deps_from_genesis_are_secp_dep_group_and_udt_code() {
        let base = base_params();
        let genesis = fake_genesis();
        let deps = cell_deps_from_genesis(&genesis, &base.udt_type_script).expect("deps");
        assert_eq!(deps.len(), 2);
        assert_eq!(
            deps[0].dep_type().as_slice(),
            &[u8::from(packed::Byte::from(DepType::DepGroup as u8))],
            "first cell dep must be the secp256k1 dep group"
        );
        let secp_dep_index: u32 = deps[0].out_point().index().unpack();
        assert_eq!(secp_dep_index, 0u32);
        assert_eq!(
            deps[1].dep_type().as_slice(),
            &[u8::from(packed::Byte::from(DepType::Code as u8))],
            "second cell dep must be the simple UDT code cell"
        );
        let udt_dep_index: u32 = deps[1].out_point().index().unpack();
        assert_eq!(udt_dep_index, 8u32);

        let wrong_udt_code_hash = base
            .udt_type_script
            .as_builder()
            .code_hash::<packed::Byte32>(H256::from([0x99_u8; 32]).pack())
            .build();
        assert!(cell_deps_from_genesis(&genesis, &wrong_udt_code_hash).is_err());
    }

    fn molecule_script_hex(code_hash: [u8; 32], hash_type_byte: u8, args: [u8; 32]) -> String {
        let mut bytes = Vec::new();
        let total: u32 = 16 + 32 + 1 + 4 + 32;
        bytes.extend(total.to_le_bytes());
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(48u32.to_le_bytes());
        bytes.extend(49u32.to_le_bytes());
        bytes.extend(code_hash);
        bytes.push(hash_type_byte);
        bytes.extend((32u32).to_le_bytes());
        bytes.extend(args);
        format!("0x{}", hex::encode(bytes))
    }

    #[test]
    fn mutation_request_round_trips_through_json() {
        let payload = format!(
            r#"{{
                "payment_hash": "0x{payment_hash}",
                "claimant_lock": "{claimant_lock}",
                "refund_lock": "{refund_lock}",
                "refund_after_lock_time": "0x5100000000012600",
                "gross_amount": "0x3e9",
                "capacity_ckb": "0x3e8",
                "liquidity_lock_code_hash": "0x{code_hash}",
                "liquidity_lock_hash_type": "data2",
                "udt_type_script": {{
                    "code_hash": "0x{udt_code_hash}",
                    "hash_type": "data2",
                    "args": "0x{udt_args}"
                }},
                "mutation": {{ "kind": "data_length", "length": 15 }}
            }}"#,
            payment_hash = hex::encode([0x11_u8; 32]),
            claimant_lock = molecule_script_hex([0x22_u8; 32], 1, [0x33_u8; 32]),
            refund_lock = molecule_script_hex([0x44_u8; 32], 0, [0x55_u8; 32]),
            code_hash = hex::encode([0x66_u8; 32]),
            udt_code_hash = hex::encode([0x77_u8; 32]),
            udt_args = hex::encode([0x88_u8; 32]),
        );
        let request = parse_request_payload(&payload).expect("parses");
        assert_eq!(
            request.mutation,
            Mutation::DataLength { length: 15 },
            "data_length payload must deserialize with the nested length"
        );
        let base = parse_base_params(&request).expect("base params");
        assert_eq!(base.gross_amount, 0x3e9);
        assert_eq!(base.capacity_ckb, 0x3e8);
        assert_eq!(base.refund_after_lock_time, 0x5100000000012600);
        let mutated = apply_mutation(&base, &request.mutation).expect("applies");
        assert_eq!(mutated.data.len(), 15);
    }

    #[test]
    fn mutation_enum_uses_snake_case_kind_field() {
        let json = serde_json::to_value(Mutation::ClaimantLockHash).expect("serializes");
        assert_eq!(json, serde_json::json!({ "kind": "claimant_lock_hash" }));
    }

    #[test]
    fn parse_secret_key_accepts_plain_key_file_format() {
        let dir = std::env::temp_dir().join("liquidity-lock-mutator-test");
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("plain_key");
        std::fs::write(
            &path,
            "0x0101010101010101010101010101010101010101010101010101010101010101\n",
        )
        .expect("write key");
        let key = parse_secret_key_file(path.to_str().expect("utf8")).expect("parses");
        assert_eq!(sender_lock_script(&key).args().len(), 20);
    }
}
