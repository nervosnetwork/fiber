use crate::{
    fiber::{
        channel::{ChannelActorStateStore, RemoveTlcCommand},
        onchain_tlc_reconcile::onchain_fulfilled_preimage,
    },
    invoice::{CkbInvoice, CkbInvoiceStatus, InvoiceStore, PreimageStore},
    utils::payment::is_invoice_fulfilled,
};
use fiber_types::{
    Hash256, RemoveTlcFulfill, RemoveTlcReason, TLCId, TlcErr, TlcErrPacket, TlcErrorCode, TlcInfo,
};

pub struct SettleTlcSetCommand<'s, S> {
    payment_hash: Hash256,
    is_hold_tlc_set: bool,
    allow_received_invoice: bool,
    tlcs: Vec<TlcSettlementContext>,
    store: &'s S,
}

pub struct SettleOnChainFulfilledInvoiceCommand<'s, S> {
    payment_hash: Hash256,
    store: &'s S,
}

#[derive(Debug)]
pub struct TlcSettlementContext {
    channel_id: Hash256,
    id: u64,
    amount: u128,
    total_amount: Option<u128>,
    shared_secret: [u8; 32],
}

impl TlcSettlementContext {
    pub fn new(tlc_info: &TlcInfo, channel_id: Hash256) -> Self {
        Self {
            channel_id,
            id: tlc_info.id(),
            amount: tlc_info.amount,
            total_amount: tlc_info.total_amount,
            shared_secret: tlc_info.shared_secret,
        }
    }

    fn total_amount_or_amount(&self) -> u128 {
        self.total_amount.unwrap_or(self.amount)
    }

    fn into_remove_tlc_fail_settlement(self, error_code: TlcErrorCode) -> TlcSettlement {
        TlcSettlement::new(
            self.channel_id,
            RemoveTlcCommand {
                id: self.id,
                reason: RemoveTlcReason::RemoveTlcFail(TlcErrPacket::new(
                    TlcErr::new(error_code),
                    &self.shared_secret,
                )),
            },
        )
    }

    fn into_remove_tlc_fulfill_settlement(self, payment_preimage: Hash256) -> TlcSettlement {
        TlcSettlement::new(
            self.channel_id,
            RemoveTlcCommand {
                id: self.id,
                reason: RemoveTlcReason::RemoveTlcFulfill(RemoveTlcFulfill { payment_preimage }),
            },
        )
    }
}

impl<'s, S> SettleTlcSetCommand<'s, S>
where
    S: PreimageStore + InvoiceStore + ChannelActorStateStore,
{
    pub fn new(payment_hash: Hash256, channel_tlc_ids: Vec<(Hash256, u64)>, store: &'s S) -> Self {
        if channel_tlc_ids.is_empty() {
            Self::new_hold_tlc_set(payment_hash, store)
        } else {
            Self::new_immediate_tlc_set(payment_hash, channel_tlc_ids, store)
        }
    }

    pub fn new_immediate_tlc_set(
        payment_hash: Hash256,
        channel_tlc_ids: Vec<(Hash256, u64)>,
        store: &'s S,
    ) -> Self {
        let tlcs: Vec<_> = channel_tlc_ids
            .into_iter()
            .filter_map(|(channel_id, tlc_id)| make_sttlement_context(channel_id, tlc_id, store))
            .collect();
        Self {
            payment_hash,
            tlcs,
            store,
            is_hold_tlc_set: false,
            allow_received_invoice: false,
        }
    }

    pub fn new_hold_tlc_set(payment_hash: Hash256, store: &'s S) -> Self {
        Self::new_hold_tlc_set_with_received_invoice(payment_hash, store, false)
    }

    pub fn new_received_hold_tlc_set(payment_hash: Hash256, store: &'s S) -> Self {
        Self::new_hold_tlc_set_with_received_invoice(payment_hash, store, true)
    }

    fn new_hold_tlc_set_with_received_invoice(
        payment_hash: Hash256,
        store: &'s S,
        allow_received_invoice: bool,
    ) -> Self {
        let tlcs = store
            .get_payment_hold_tlcs(payment_hash)
            .iter()
            .filter_map(|hold_tlc| {
                make_sttlement_context(hold_tlc.channel_id, hold_tlc.tlc_id, store)
            })
            .collect();

        Self {
            payment_hash,
            tlcs,
            store,
            is_hold_tlc_set: true,
            allow_received_invoice,
        }
    }

    pub fn run(mut self) -> Vec<TlcSettlement> {
        let (Some(invoice), Some(invoice_status)) = (
            self.store.get_invoice(&self.payment_hash),
            self.store.get_invoice_status(&self.payment_hash),
        ) else {
            // TLC without invoice should not be settled via SettleTlcSetCommand, reject them as invoice
            // canceled.
            return self.reject_all(TlcErrorCode::InvoiceCancelled);
        };

        if let Err(error_code) = self.verify(&invoice, &invoice_status) {
            return self.reject_all(error_code);
        }

        if self.should_skip_received_hold_set(&invoice_status) {
            return self.skip_all();
        }

        let mut rejected = self.leave_just_fulfilled_tlcs(&invoice);
        if self.tlcs.is_empty() {
            return rejected;
        }

        // Now we are sure the invoice is fulfilled, and `self.tlcs` is ready to be settled.
        // Update invoice status to Received
        self.mark_invoice_as_received_if_still_open(&invoice_status);

        let mut settlements = self.try_settle_all();
        settlements.append(&mut rejected);
        settlements
    }

    /// Leave just fulfilled tlcs, and reject the rest.
    ///
    /// Return settlements for rejected tlcs.
    ///
    /// When the invoice is not fulfilled:
    /// - If invoice allows mpp, clear tlcs and return empty settlements,
    /// - Otherwise, reject all tlcs and return settlements for rejected tlcs.
    ///
    /// When this function returns, and `self.tlcs` is not empty, it means the
    /// invoice is now fulfilled, and `self.tlcs` is ready to be settled.
    fn leave_just_fulfilled_tlcs(&mut self, invoice: &CkbInvoice) -> Vec<TlcSettlement> {
        if invoice.allow_mpp() {
            self.leave_just_fulfilled_tlcs_for_mpp_invoice(invoice)
        } else {
            self.leave_just_fulfilled_tlcs_for_non_mpp_invoice(invoice)
        }
    }

    fn leave_just_fulfilled_tlcs_for_mpp_invoice(
        &mut self,
        invoice: &CkbInvoice,
    ) -> Vec<TlcSettlement> {
        let Some(first_tlc) = self.tlcs.first() else {
            return Vec::new();
        };

        let total_amount = first_tlc.total_amount_or_amount();
        if total_amount < invoice.amount.unwrap_or_default() {
            return self.reject_all(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
        }

        let mut accumulated_amount = 0;
        // Remove overpaid TLCs
        let mut retain_len: usize = 0;
        for tlc in self.tlcs.iter() {
            if accumulated_amount < total_amount {
                accumulated_amount = accumulated_amount.saturating_add(tlc.amount);
                retain_len += 1;
            }
        }

        // If not fulfilled, clear tlcs and return empty settlements.
        if accumulated_amount < total_amount {
            self.tlcs.clear();
            Vec::new()
        } else {
            let overpaid_tlcs = self.tlcs.split_off(retain_len);
            self.reject_tlcs(overpaid_tlcs, TlcErrorCode::HoldTlcTimeout)
        }
    }

    fn leave_just_fulfilled_tlcs_for_non_mpp_invoice(
        &mut self,
        invoice: &CkbInvoice,
    ) -> Vec<TlcSettlement> {
        let required_amount = invoice.amount.unwrap_or_default();

        let Some(index) = self
            .tlcs
            .iter()
            .position(|tlc| tlc.amount >= required_amount)
        else {
            // No one fulfilled, reject all
            return self.reject_all(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
        };

        let mut rejected_tlcs = std::mem::take(&mut self.tlcs);
        self.tlcs.push(rejected_tlcs.swap_remove(index));
        self.reject_tlcs(rejected_tlcs, TlcErrorCode::HoldTlcTimeout)
    }

    /// Verify TLCs against the invoice.
    ///
    /// Returns `Err` to reject all tlcs with the error code.
    fn verify(
        &self,
        invoice: &CkbInvoice,
        invoice_status: &CkbInvoiceStatus,
    ) -> Result<(), TlcErrorCode> {
        self.verify_invoice_status(invoice, invoice_status)?;
        self.verify_mpp_tlcs_have_consistent_total_amount(invoice)?;
        Ok(())
    }

    fn verify_invoice_status(
        &self,
        invoice: &CkbInvoice,
        invoice_status: &CkbInvoiceStatus,
    ) -> Result<(), TlcErrorCode> {
        match invoice_status {
            CkbInvoiceStatus::Open => {
                // Check if Open invoice is expired by its expiry field.
                if invoice.is_expired() {
                    Err(TlcErrorCode::InvoiceExpired)
                } else {
                    Ok(())
                }
            }
            CkbInvoiceStatus::Received => {
                if self.is_hold_tlc_set {
                    Ok(())
                } else {
                    Err(TlcErrorCode::HoldTlcTimeout)
                }
            }
            CkbInvoiceStatus::Expired => Err(TlcErrorCode::InvoiceExpired),
            CkbInvoiceStatus::Cancelled => Err(TlcErrorCode::InvoiceCancelled),
            CkbInvoiceStatus::Paid => Err(TlcErrorCode::HoldTlcTimeout),
        }
    }

    fn should_skip_received_hold_set(&self, invoice_status: &CkbInvoiceStatus) -> bool {
        // Multiple channel actors can enqueue SettleHoldTlcSet for the same MPP hold invoice.
        // Once one command marks the invoice Received, later stale commands must not reject the
        // already-held TLCs. Preimage reveal uses the explicit received-hold path below.
        *invoice_status == CkbInvoiceStatus::Received
            && self.is_hold_tlc_set
            && !self.allow_received_invoice
    }

    fn verify_mpp_tlcs_have_consistent_total_amount(
        &self,
        invoice: &CkbInvoice,
    ) -> Result<(), TlcErrorCode> {
        if invoice.allow_mpp()
            && self.tlcs.len() > 1
            && !self
                .tlcs
                .windows(2)
                .all(|w| w[0].total_amount == w[1].total_amount)
        {
            tracing::error!("TLCs have inconsistent total_amount");
            return Err(TlcErrorCode::IncorrectOrUnknownPaymentDetails);
        }
        Ok(())
    }

    fn try_settle_all(self) -> Vec<TlcSettlement> {
        if let Some(payment_preimage) = self.store.get_preimage(&self.payment_hash) {
            self.tlcs
                .into_iter()
                .map(|tlc| tlc.into_remove_tlc_fulfill_settlement(payment_preimage))
                .collect()
        } else {
            // Skip this time and retry later when the preimage is available.
            self.skip_all()
        }
    }

    fn reject_tlcs(
        &self,
        tlcs: Vec<TlcSettlementContext>,
        error_code: TlcErrorCode,
    ) -> Vec<TlcSettlement> {
        tracing::error!(
            "Remove TLCs for payment hash {} because of error {:?}",
            self.payment_hash,
            error_code,
        );
        tracing::debug!(
            "Removed TLCs for payment hash {} because of error {:?}: {:?}",
            self.payment_hash,
            error_code,
            tlcs,
        );
        tlcs.into_iter()
            .map(|tlc| tlc.into_remove_tlc_fail_settlement(error_code))
            .collect()
    }

    fn reject_all(&mut self, error_code: TlcErrorCode) -> Vec<TlcSettlement> {
        let tlcs = std::mem::take(&mut self.tlcs);
        self.reject_tlcs(tlcs, error_code)
    }

    fn skip_all(self) -> Vec<TlcSettlement> {
        Vec::new()
    }

    fn mark_invoice_as_received_if_still_open(&self, invoice_status: &CkbInvoiceStatus) {
        if *invoice_status == CkbInvoiceStatus::Open {
            self.store
                .update_invoice_status(&self.payment_hash, CkbInvoiceStatus::Received)
                .expect("update invoice status failed");
        }
    }
}

impl<'s, S> SettleOnChainFulfilledInvoiceCommand<'s, S>
where
    S: InvoiceStore + ChannelActorStateStore,
{
    pub fn new(payment_hash: Hash256, store: &'s S) -> Self {
        Self {
            payment_hash,
            store,
        }
    }

    pub fn run(self) -> bool {
        let Some(invoice) = self.store.get_invoice(&self.payment_hash) else {
            return false;
        };
        if self.store.get_invoice_status(&self.payment_hash) == Some(CkbInvoiceStatus::Paid) {
            return false;
        }

        let fulfilled_received_tlcs =
            collect_onchain_fulfilled_received_tlcs(self.store, self.payment_hash);
        if invoice.allow_mpp()
            && fulfilled_received_tlcs.len() > 1
            && !fulfilled_received_tlcs
                .windows(2)
                .all(|w| w[0].total_amount == w[1].total_amount)
        {
            tracing::error!(
                "On-chain fulfilled TLCs have inconsistent total_amount for payment hash {}",
                self.payment_hash
            );
            return false;
        }

        if is_invoice_fulfilled(&invoice, fulfilled_received_tlcs.iter()) {
            self.store
                .update_invoice_status(&self.payment_hash, CkbInvoiceStatus::Paid)
                .expect("update invoice status failed");
            true
        } else {
            false
        }
    }
}

fn collect_onchain_fulfilled_received_tlcs(
    store: &impl ChannelActorStateStore,
    payment_hash: Hash256,
) -> Vec<TlcInfo> {
    store
        .get_channel_states(None)
        .into_iter()
        .filter_map(|(_, channel_id, _)| store.get_channel_actor_state(&channel_id))
        .flat_map(|state| {
            let channel_id = state.get_id();
            state
                .tlc_state
                .received_tlcs
                .tlcs
                .clone()
                .into_iter()
                .map(move |tlc| (channel_id, tlc))
        })
        .filter_map(|(channel_id, tlc)| {
            if tlc.payment_hash != payment_hash {
                return None;
            }
            let Some(RemoveTlcReason::RemoveTlcFulfill(fulfill)) = &tlc.removed_reason else {
                return None;
            };
            let preimage = onchain_fulfilled_preimage(&channel_id, store, &tlc)?;
            (preimage == fulfill.payment_preimage).then_some(tlc)
        })
        .collect()
}

fn make_sttlement_context<S: ChannelActorStateStore>(
    channel_id: Hash256,
    tlc_id: u64,
    store: &S,
) -> Option<TlcSettlementContext> {
    let state = store.get_channel_actor_state(&channel_id)?;
    let tlc_id = TLCId::Received(tlc_id);
    state
        .get_received_tlc(tlc_id)
        .map(|tlc_info| TlcSettlementContext::new(tlc_info, channel_id))
}

pub struct TlcSettlement {
    channel_id: Hash256,
    remove_tlc_command: RemoveTlcCommand,
}

impl TlcSettlement {
    pub fn new(channel_id: Hash256, remove_tlc_command: RemoveTlcCommand) -> Self {
        Self {
            channel_id,
            remove_tlc_command,
        }
    }

    pub fn channel_id(&self) -> Hash256 {
        self.channel_id
    }

    pub fn tlc_id(&self) -> u64 {
        self.remove_tlc_command.id
    }

    pub fn remove_tlc_command(&self) -> &RemoveTlcCommand {
        &self.remove_tlc_command
    }
}
