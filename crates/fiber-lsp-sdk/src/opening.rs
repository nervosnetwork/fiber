//! Client-owned opening intent and validation of the frozen tenant proposal.
use ckb_types::{
    core::TransactionBuilder,
    packed::{CellDep, CellInput, CellOutput, Script},
    prelude::*,
};
use fiber_json_types::{
    OpenChannelWithExternalFundingParams, OpenChannelWithExternalFundingResult,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{commitment::invalid, CommitmentParameters, SignerError};

/// Trusted network configuration, supplied by the wallet rather than the LSP.
#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpeningNetwork {
    /// FundingLock deployment, with empty arguments.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub funding_lock: Script,
    /// CommitmentLock deployment, with empty arguments.
    #[serde_as(as = "fiber_types::EntityHex")]
    pub commitment_lock: Script,
    /// Number of deps in a commitment transaction for this asset.
    pub commitment_cell_deps: usize,
    /// Trusted epoch duration in milliseconds.
    pub epoch_duration_ms: u64,
    /// Protocol reserve added to occupied settlement capacity, in shannons.
    pub minimum_shutdown_fee: u64,
}

/// Transport for the production tenant opening RPC. No local approvals are RPCs.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait TenantOpeningRpc: Sync {
    /// Send the saved opening intent using the tenant's credentials.
    async fn open_channel_with_external_funding(
        &self,
        token: &str,
        request: OpenChannelWithExternalFundingParams,
    ) -> Result<OpenChannelWithExternalFundingResult, crate::SessionError>;
}

/// Independent wallet validation of the complete funding transaction.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait FundingVerifier: Sync {
    /// Resolve inputs independently and enforce wallet debit, asset and fee limits.
    /// Return the exact approved inputs in transaction order. Never approve inputs
    /// merely because the LSP included them in its response.
    async fn verify_funding(
        &self,
        request: &OpenChannelWithExternalFundingParams,
        result: &OpenChannelWithExternalFundingResult,
    ) -> Result<Vec<ckb_types::packed::OutPoint>, crate::SessionError>;
}

/// Durable session storage used before network I/O and before returning success.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait OpeningPersistence: Sync {
    /// Atomically persist the session map; errors stop opening progression.
    async fn save_session(
        &self,
        state: &crate::HostedSessionState,
    ) -> Result<(), crate::SessionError>;
}

impl<S: crate::SignerStore> crate::ChannelSigner<S> {
    /// Validate the tenant proposal against the original wallet request and trusted network.
    /// The funding wallet must approve the complete transaction, including its net
    /// debit and fee; `expected_inputs` binds that approval to the selected cells.
    pub(crate) async fn verify_and_record_opening(
        &self,
        request: &OpenChannelWithExternalFundingParams,
        result: &OpenChannelWithExternalFundingResult,
        network: &OpeningNetwork,
        expected_inputs: &[ckb_types::packed::OutPoint],
    ) -> Result<(), SignerError> {
        if request.public != Some(false) {
            return Err(invalid("tenant opening requires public: false"));
        }
        let expected = crate::json::open_material_to_rpc(&self.channel_open_material(false).await?);
        if serde_json::to_value(Some(expected)).map_err(|e| invalid(&e.to_string()))?
            != serde_json::to_value(&request.external_channel_signer)
                .map_err(|e| invalid(&e.to_string()))?
        {
            return Err(invalid("opening request does not belong to this signer"));
        }
        let parameters = validate_opening(request, result, network)?;
        self.approve_opening(
            &result.unsigned_funding_tx.clone().into(),
            0,
            request.shutdown_script.clone().into(),
            expected_inputs,
            parameters,
        )
        .await
    }
}

fn reserve(
    script: Script,
    asset: Option<Script>,
    network: &OpeningNetwork,
) -> Result<u64, SignerError> {
    let lock = if script.args().len() < 58 {
        Script::new_builder().args([0u8; 58].pack()).build()
    } else {
        script
    };
    let data = if asset.is_some() { 16 } else { 0 };
    let capacity = CellOutput::new_builder()
        .lock(lock)
        .type_(asset.pack())
        .build()
        .occupied_capacity(
            ckb_types::core::Capacity::bytes(data).map_err(|e| invalid(&e.to_string()))?,
        )
        .map_err(|e| invalid(&e.to_string()))?
        .as_u64();
    capacity
        .checked_add(network.minimum_shutdown_fee)
        .ok_or_else(|| invalid("reserve overflow"))
}

pub(crate) fn validate_opening(
    request: &OpenChannelWithExternalFundingParams,
    result: &OpenChannelWithExternalFundingResult,
    network: &OpeningNetwork,
) -> Result<CommitmentParameters, SignerError> {
    let context = result
        .opening_context
        .as_ref()
        .ok_or_else(|| invalid("opening response is missing opening_context"))?;
    if !network.funding_lock.args().is_empty() || !network.commitment_lock.args().is_empty() {
        return Err(invalid("network contract scripts must have empty args"));
    }
    let funding: ckb_types::packed::Transaction = result.unsigned_funding_tx.clone().into();
    let output = funding
        .raw()
        .outputs()
        .get(0)
        .ok_or_else(|| invalid("missing funding output"))?;
    let asset: Option<Script> = request.funding_udt_type_script.clone().map(Into::into);
    if output.type_().to_opt() != asset || context.local_amount != request.funding_amount {
        return Err(invalid(
            "opening asset or local balance differs from requested funding",
        ));
    }
    if output.lock().code_hash() != network.funding_lock.code_hash()
        || output.lock().hash_type() != network.funding_lock.hash_type()
    {
        return Err(invalid("funding output uses an untrusted contract"));
    }
    // Counterparty inputs can have their own change outputs. The external funding
    // wallet approves the complete transaction before this opening is installed;
    // commitment validation must not assume every change output belongs to us.
    let delay = request
        .commitment_delay_epoch
        .ok_or_else(|| invalid("client must choose commitment delay"))?;
    let fee_rate = request
        .commitment_fee_rate
        .ok_or_else(|| invalid("client must choose commitment fee rate"))?;
    if context.commitment_delay_epoch != delay || context.commitment_fee_rate != fee_rate {
        return Err(invalid(
            "LSP changed requested commitment delay or fee rate",
        ));
    }
    let local_shutdown: Script = request.shutdown_script.clone().into();
    let remote_shutdown: Script = context.remote_shutdown_script.clone().into();
    if context.local_reserved_ckb_amount != reserve(local_shutdown, asset.clone(), network)?
        || context.remote_reserved_ckb_amount
            != reserve(remote_shutdown.clone(), asset.clone(), network)?
    {
        return Err(invalid("opening reserve differs from protocol reserve"));
    }
    let local_key = fiber_types::Pubkey::try_from(
        request
            .external_channel_signer
            .as_ref()
            .ok_or_else(|| invalid("opening requires external_channel_signer"))?
            .base_public_keys
            .tlc_base_key,
    )
    .map_err(|e| invalid(&e))?;
    let remote_key =
        fiber_types::Pubkey::try_from(context.remote_settlement_key).map_err(|e| invalid(&e))?;
    let mut keys = [local_key.serialize(), remote_key.serialize()];
    keys.sort();
    let channel_id: fiber_types::Hash256 =
        fiber_types::blake2b_hash_with_salt(&keys.concat(), &[]).into();
    if channel_id != result.channel_id.into() {
        return Err(invalid(
            "channel id does not match the opening settlement keys",
        ));
    }
    let commitment_output = CellOutput::new_builder()
        .lock(
            network
                .commitment_lock
                .clone()
                .as_builder()
                .args([0u8; 58].pack())
                .build(),
        )
        .type_(asset.clone().pack())
        .build();
    let data = if asset.is_some() {
        0u128.to_le_bytes().to_vec()
    } else {
        vec![]
    };
    let tx = TransactionBuilder::default()
        .cell_deps(vec![CellDep::default(); network.commitment_cell_deps])
        .input(CellInput::default())
        .output(commitment_output)
        .output_data(data.pack())
        .witness([0u8; 112].pack())
        .build();
    let fee = (u128::from(fee_rate) * tx.data().serialized_size_in_block() as u128) / 1000;
    Ok(CommitmentParameters {
        remote_funding_key: fiber_types::Pubkey::try_from(context.remote_funding_key)
            .map_err(|e| invalid(&e))?,
        remote_settlement_key: remote_key,
        remote_shutdown_script: remote_shutdown,
        commitment_lock: network.commitment_lock.clone(),
        delay_epoch: (0xa000_0000_0000_0000u64 | delay.value()).to_le_bytes(),
        epoch_duration_ms: network.epoch_duration_ms,
        commitment_fee: u64::try_from(fee).map_err(|_| invalid("commitment fee overflow"))?,
        local_amount: context.local_amount,
        remote_amount: context.remote_amount,
        local_reserved_ckb_amount: context.local_reserved_ckb_amount,
        remote_reserved_ckb_amount: context.remote_reserved_ckb_amount,
    })
}

#[cfg(test)]
pub(crate) mod tests;
