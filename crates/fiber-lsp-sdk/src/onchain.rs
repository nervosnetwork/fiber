//! Chain-backed validation of settlement and TLC withdrawals.
use ckb_types::{
    packed::{CellDep, CellInput, CellOutput, OutPoint, Script},
    prelude::*,
};
use fiber_types::{settlement_data_to_witness, Hash256};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    commitment::invalid, OnchainKeyPurpose, OnchainSigningContent, RecoveryRecord, SignerError,
};

/// Cell resolved by a wallet-controlled chain source, never by the LSP signing response.
#[derive(Clone, Debug)]
pub struct VerifiedCell {
    /// Actual live output.
    pub output: CellOutput,
    /// Actual output data.
    pub data: Vec<u8>,
}

/// Trusted chain boundary. Implementations must authenticate chain data and account for reorgs.
/// Returning success from an LSP status without independent verification is not an implementation.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait ChainVerifier: Sync {
    /// Resolve an actually live cell on the trusted chain.
    async fn live_cell(&self, outpoint: &OutPoint) -> Result<VerifiedCell, SignerError>;
    /// Verify the first input is the source commitment cell or a contract-valid descendant.
    async fn verify_commitment_lineage(
        &self,
        source: Hash256,
        outpoint: &OutPoint,
    ) -> Result<(), SignerError>;
    /// Verify all input since constraints against the trusted chain tip (including relative age).
    async fn verify_maturity(&self, inputs: &[CellInput]) -> Result<(), SignerError>;
    /// Trusted chain median time in milliseconds, for absolute TLC expiry checks.
    async fn median_time_ms(&self) -> Result<u64, SignerError>;
}

/// Wallet-approved destinations, fee and auxiliary inputs for one on-chain spend.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OnchainSpendAuthorization {
    /// Exact previously verified commitment from which the spend descends.
    pub source: crate::CommitmentReference,
    /// Wallet-approved withdrawal/change destination.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub destination: Script,
    /// Exact total CKB fee, including any auxiliary inputs.
    pub fee: u64,
    /// Exact auxiliary inputs, in transaction order. They must be plain CKB cells.
    #[serde_as(as = "Vec<fiber_types::EntityHex>")]
    pub additional_inputs: Vec<OutPoint>,
    /// Exact dependencies from trusted network configuration.
    #[serde_as(as = "Vec<fiber_types::EntityHex>")]
    pub cell_deps: Vec<CellDep>,
}

pub(crate) async fn validate<C: ChainVerifier>(
    content: &OnchainSigningContent,
    authorization: &OnchainSpendAuthorization,
    record: &RecoveryRecord,
    chain: &C,
) -> Result<(), SignerError> {
    let tx = &content.transaction;
    let raw = tx.raw();
    if raw.version() != 0u32.pack()
        || !raw.header_deps().is_empty()
        || raw.cell_deps().into_iter().collect::<Vec<_>>() != authorization.cell_deps
        || raw.inputs().len() != authorization.additional_inputs.len() + 1
        || raw.outputs().len() != raw.outputs_data().len()
    {
        return Err(invalid(
            "invalid on-chain transaction structure or dependencies",
        ));
    }
    let inputs: Vec<_> = raw.inputs().into_iter().collect();
    let first = inputs
        .first()
        .ok_or_else(|| invalid("missing commitment input"))?;
    chain
        .verify_commitment_lineage(authorization.source.tx_hash, &first.previous_output())
        .await?;
    chain.verify_maturity(&inputs).await?;
    let source = chain.live_cell(&first.previous_output()).await?;
    let original = record
        .transaction
        .raw()
        .outputs()
        .get(0)
        .ok_or_else(|| invalid("missing original output"))?;
    if source.output.type_().to_opt().is_none() && !source.data.is_empty() {
        return Err(invalid("unexpected plain CKB input data"));
    }
    let source_lock = source.output.lock();
    let args = source_lock.args().raw_data();
    let original_args = original.lock().args().raw_data();
    if args.len() != 57
        || original_args.len() != 57
        || args[..36] != original_args[..36]
        || source_lock.code_hash() != original.lock().code_hash()
        || source_lock.hash_type() != original.lock().hash_type()
        || source.output.type_() != original.type_()
        || args[56] > 1
    {
        return Err(invalid("on-chain source differs from verified commitment"));
    }
    let witness = tx
        .witnesses()
        .get(0)
        .ok_or_else(|| invalid("missing settlement witness"))?
        .raw_data();
    const PREFIX: [u8; 16] = [16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0, 16, 0, 0, 0];
    if witness.len() < 18 || witness[..16] != PREFIX || witness[16] != 1 {
        return Err(invalid("expected exactly one settlement unlock"));
    }
    let count = usize::from(witness[17]);
    let body_len = 1 + count * 85 + 72;
    let body = witness
        .get(17..17 + body_len)
        .ok_or_else(|| invalid("truncated settlement witness"))?;
    if fiber_types::blake2b_hash_with_salt(body, &[])[..20] != args[36..56] {
        return Err(invalid("settlement witness is not committed by live input"));
    }
    let initial = settlement_data_to_witness(
        &record.settlement.data,
        record.reference.for_remote,
        record.settlement.local_settlement_key,
        record.settlement.remote_settlement_key,
    );
    // Every remaining TLC must be an unchanged member of the original signed snapshot.
    let original_tlcs: Vec<_> = initial[1..1 + usize::from(initial[0]) * 85]
        .chunks_exact(85)
        .collect();
    let remaining: Vec<_> = body[1..1 + count * 85].chunks_exact(85).collect();
    for (i, entry) in remaining.iter().enumerate() {
        if !original_tlcs.contains(entry) || remaining[..i].contains(entry) {
            return Err(invalid("unexpected or duplicate remaining TLC"));
        }
    }
    let balances = &body[1 + count * 85..];
    let initial_balances = &initial[1 + usize::from(initial[0]) * 85..];
    for offset in [0, 36] {
        if balances[offset..offset + 36] != initial_balances[offset..offset + 36]
            && balances[offset..offset + 36] != [0; 36]
        {
            return Err(invalid(
                "on-chain settlement balance differs from signed snapshot",
            ));
        }
    }
    let unlock = &witness[17 + body_len..];
    if unlock.len() != 67 && unlock.len() != 99 {
        return Err(invalid("invalid unlock length"));
    }
    if unlock[1] > 1 || (unlock[1] == 1) != (unlock.len() == 99) || unlock[2..67] != [0; 65] {
        return Err(invalid("invalid unsigned unlock template"));
    }
    let now = chain.median_time_ms().await?;
    let local_offset = if record.reference.for_remote { 36 } else { 0 };
    let mut next = body.to_vec();
    let (amount, fraction) = if unlock[0] >= 0xfe {
        let expected = if record.reference.for_remote {
            0xff
        } else {
            0xfe
        };
        if content.key_purpose != OnchainKeyPurpose::Settlement
            || unlock[0] != expected
            || unlock[1] != 0
            || balances[local_offset..local_offset + 20] == [0; 20]
        {
            return Err(invalid("settlement unlock does not belong to signer"));
        }
        if remaining.iter().any(|t| {
            let expiry = u64::from_le_bytes(t[77..85].try_into().unwrap()) & 0x00ff_ffff_ffff_ffff;
            now / 1000 <= expiry
        }) {
            return Err(invalid("settlement attempted with unexpired TLCs"));
        }
        let amount = u128::from_le_bytes(
            balances[local_offset + 20..local_offset + 36]
                .try_into()
                .unwrap(),
        );
        next[1 + count * 85 + local_offset..1 + count * 85 + local_offset + 36].fill(0);
        (amount, 3u128)
    } else {
        let index = usize::from(unlock[0]);
        let entry = remaining
            .get(index)
            .ok_or_else(|| invalid("invalid TLC unlock index"))?;
        let original_index = original_tlcs
            .iter()
            .position(|t| t == entry)
            .ok_or_else(|| invalid("untracked TLC"))?;
        let tlc = &record.settlement.data.tlcs[original_index];
        let inbound = tlc.tlc_id.is_received() == record.reference.for_remote;
        let derivation = tlc
            .local_key_commitment_number
            .ok_or_else(|| invalid("missing TLC derivation"))?;
        if content.key_purpose
            != (OnchainKeyPurpose::Tlc {
                commitment_number: derivation,
            })
        {
            return Err(invalid("wrong TLC signing key"));
        }
        if inbound {
            if unlock[1] != 1
                || tlc.hash_algorithm.hash(&unlock[67..99]) != *tlc.payment_hash.as_ref()
            {
                return Err(invalid("invalid TLC fulfillment preimage"));
            }
        } else if unlock[1] != 0 || now / 1000 <= tlc.expiry / 1000 {
            return Err(invalid("TLC timeout has not matured"));
        } else if !inputs.iter().any(|input| {
            let since: u64 = input.since().unpack();
            since >> 56 == 0x40 && (since & 0x00ff_ffff_ffff_ffff) > tlc.expiry / 1000
        }) {
            return Err(invalid("TLC timeout requires an absolute timestamp input"));
        }
        next.drain(1 + index * 85..1 + (index + 1) * 85);
        next[0] -= 1;
        (tlc.payment_amount, if inbound { 1u128 } else { 2u128 })
    };
    if args[56] == 0 {
        let required = u64::from_le_bytes(args[20..28].try_into().unwrap());
        let actual: u64 = first.since().unpack();
        if !epoch_at_least(actual, required, fraction, 3) {
            return Err(invalid("insufficient on-chain recovery delay"));
        }
    } else if first.since() != 0u64.pack() {
        return Err(invalid("unexpected descendant since"));
    }
    let mut total_capacity: u64 = source.output.capacity().unpack();
    for (input, approved) in inputs.iter().skip(1).zip(&authorization.additional_inputs) {
        if input.previous_output() != *approved
            || *approved == first.previous_output()
            || authorization
                .additional_inputs
                .iter()
                .filter(|p| *p == approved)
                .count()
                != 1
        {
            return Err(invalid("unexpected or duplicate fee input"));
        }
        let cell = chain.live_cell(approved).await?;
        if cell.output.type_().to_opt().is_some()
            || !cell.data.is_empty()
            || cell.output.lock() != authorization.destination
        {
            return Err(invalid("fee input is not approved plain CKB"));
        }
        let capacity: u64 = cell.output.capacity().unpack();
        total_capacity = total_capacity
            .checked_add(capacity)
            .ok_or_else(|| invalid("capacity overflow"))?;
    }
    let udt = source.output.type_().to_opt().is_some();
    let source_capacity: u64 = source.output.capacity().unpack();
    let remote_offset = 36 - local_offset;
    let final_party = unlock[0] >= 0xfe && balances[remote_offset..remote_offset + 20] == [0; 20];
    let source_tokens = if udt {
        u128::from_le_bytes(
            source
                .data
                .as_slice()
                .try_into()
                .map_err(|_| invalid("invalid token data"))?,
        )
    } else {
        u128::from(source_capacity)
    };
    let payout = if final_party {
        source_tokens
    } else {
        amount.min(source_tokens)
    };
    if !final_party && payout != amount {
        return Err(invalid("claim exceeds live cell balance"));
    }
    let mut expected_remaining = source.output.clone();
    let mut next_args = args[..36].to_vec();
    next_args.extend_from_slice(&fiber_types::blake2b_hash_with_salt(&next, &[])[..20]);
    next_args.push(1);
    expected_remaining = expected_remaining
        .as_builder()
        .lock(source_lock.as_builder().args(next_args.pack()).build())
        .capacity(if udt {
            if unlock[0] >= 0xfe {
                source
                    .output
                    .occupied_capacity(
                        ckb_types::core::Capacity::bytes(16)
                            .map_err(|_| invalid("capacity overflow"))?,
                    )
                    .map_err(|_| invalid("capacity overflow"))?
                    .as_u64()
            } else {
                source_capacity
            }
        } else {
            source_capacity
                .checked_sub(u64::try_from(payout).map_err(|_| invalid("payout overflow"))?)
                .ok_or_else(|| invalid("negative remaining capacity"))?
        })
        .build();
    let remainder_data = if udt {
        source_tokens
            .checked_sub(payout)
            .ok_or_else(|| invalid("negative token remainder"))?
            .to_le_bytes()
            .to_vec()
    } else {
        vec![]
    };
    let mut remainder_seen = final_party;
    let mut destination_tokens = 0u128;
    let mut output_capacity = 0u64;
    let mut destinations = 0;
    for (i, output) in raw.outputs().into_iter().enumerate() {
        let data = raw
            .outputs_data()
            .get(i)
            .ok_or_else(|| invalid("missing output data"))?
            .raw_data();
        let capacity: u64 = output.capacity().unpack();
        output_capacity = output_capacity
            .checked_add(capacity)
            .ok_or_else(|| invalid("output overflow"))?;
        if !remainder_seen && output == expected_remaining && data.as_ref() == remainder_data {
            remainder_seen = true;
            continue;
        }
        if output.lock() != authorization.destination {
            return Err(invalid("unapproved on-chain destination"));
        }
        destinations += 1;
        if udt && output.type_() == source.output.type_() {
            destination_tokens = destination_tokens
                .checked_add(u128::from_le_bytes(
                    data.as_ref()
                        .try_into()
                        .map_err(|_| invalid("invalid payout data"))?,
                ))
                .ok_or_else(|| invalid("token overflow"))?;
        } else if output.type_().to_opt().is_some() || !data.is_empty() {
            return Err(invalid("unexpected output asset"));
        }
    }
    if !remainder_seen
        || destinations == 0
        || destinations > 2
        || total_capacity.checked_sub(output_capacity) != Some(authorization.fee)
        || (udt && destination_tokens != payout)
    {
        return Err(invalid(
            "on-chain outputs do not conserve authorized funds and fee",
        ));
    }
    Ok(())
}

fn epoch_at_least(actual: u64, required: u64, numerator: u128, denominator: u128) -> bool {
    if actual >> 56 != 0xa0 || required >> 56 != 0xa0 {
        return false;
    }
    let parts = |value| {
        let e = ckb_types::core::EpochNumberWithFraction::from_full_value(
            value & 0x00ff_ffff_ffff_ffff,
        );
        if e.length() == 0 || e.index() >= e.length() {
            None
        } else {
            Some((
                u128::from(e.number()) * u128::from(e.length()) + u128::from(e.index()),
                u128::from(e.length()),
            ))
        }
    };
    match (parts(actual), parts(required)) {
        (Some((a, b)), Some((c, d))) => a * d * denominator >= c * b * numerator,
        _ => false,
    }
}
