//! Payment request models for client-side Loop Out execution.

use std::time::Duration;

use fiber_types::{Hash256, PaymentStatus, Pubkey};
use ractor::{call, ActorRef};

use crate::fiber::network::SendPaymentResponse;
use crate::fiber::payment::SendPaymentCommand;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::invoice::{CkbInvoiceStatus, Currency, InvoiceBuilder, InvoiceError};
use crate::liquidity::actor::{LoopOutPaymentAdapter, LoopOutPaymentStatus};
use crate::liquidity::types::{
    loop_out_gross_payment_amount, loop_out_payment_principal, LiquidityLoopOutError,
};

/// Fiber payment request derived from accepted Loop Out terms.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutPaymentRequest {
    /// Payment hash identifying the HTLC/conditional payment.
    pub payment_hash: Hash256,
    /// Node pubkey receiving the Fiber payment.
    pub target_pubkey: Option<Pubkey>,
    /// Invoice to pay when the receiver is encoded by the invoice.
    pub invoice: Option<String>,
    /// Fiber payment principal including the provider fee but excluding routing fees.
    pub amount: u128,
    /// Maximum Fiber routing fee the client accepts for this payment.
    pub max_fee_amount: u128,
}

impl LoopOutPaymentRequest {
    /// Build a Loop Out payment request from the net on-chain amount and fee limits.
    pub fn new(
        payment_hash: Hash256,
        target_pubkey: Pubkey,
        amount: u128,
        provider_fee: u128,
        routing_fee_limit: u128,
    ) -> Result<Self, LiquidityLoopOutError> {
        loop_out_gross_payment_amount(amount, provider_fee, routing_fee_limit)?;
        Ok(Self {
            payment_hash,
            target_pubkey: Some(target_pubkey),
            invoice: None,
            amount: loop_out_payment_principal(amount, provider_fee)?,
            max_fee_amount: routing_fee_limit,
        })
    }

    /// Build an invoice-based payment request for provider-side Loop In execution.
    pub fn new_invoice(
        payment_hash: Hash256,
        invoice: String,
        amount: u128,
        max_fee_amount: u128,
    ) -> Self {
        Self {
            payment_hash,
            target_pubkey: None,
            invoice: Some(invoice),
            amount,
            max_fee_amount,
        }
    }
}

/// Payment adapter that sends Loop Out payments through the existing Fiber network actor.
#[derive(Clone)]
pub struct NetworkLoopOutPaymentAdapter {
    network_actor: ActorRef<NetworkActorMessage>,
    polling_policy: NetworkLoopOutPaymentPollingPolicy,
    currency: Currency,
}

/// Bounded polling policy for waiting on a sent Loop Out payment to settle.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct NetworkLoopOutPaymentPollingPolicy {
    /// Maximum number of `GetPayment` reloads after `SendPayment` returns in-flight.
    pub max_reload_attempts: u32,
    /// Delay between non-terminal reload responses.
    pub reload_interval: Duration,
}

impl Default for NetworkLoopOutPaymentPollingPolicy {
    fn default() -> Self {
        Self {
            max_reload_attempts: 60,
            reload_interval: Duration::from_secs(1),
        }
    }
}

impl NetworkLoopOutPaymentAdapter {
    /// Create a network-backed Loop Out payment adapter.
    pub fn new(network_actor: ActorRef<NetworkActorMessage>) -> Self {
        Self::with_currency(network_actor, Currency::Fibd)
    }

    /// Create a network-backed Loop Out payment adapter for a specific invoice currency.
    pub fn with_currency(network_actor: ActorRef<NetworkActorMessage>, currency: Currency) -> Self {
        Self::with_polling_policy_and_currency(
            network_actor,
            NetworkLoopOutPaymentPollingPolicy::default(),
            currency,
        )
    }

    /// Create a network-backed Loop Out payment adapter with a custom polling policy.
    pub fn with_polling_policy(
        network_actor: ActorRef<NetworkActorMessage>,
        polling_policy: NetworkLoopOutPaymentPollingPolicy,
    ) -> Self {
        Self::with_polling_policy_and_currency(network_actor, polling_policy, Currency::Fibd)
    }

    fn with_polling_policy_and_currency(
        network_actor: ActorRef<NetworkActorMessage>,
        polling_policy: NetworkLoopOutPaymentPollingPolicy,
        currency: Currency,
    ) -> Self {
        Self {
            network_actor,
            polling_policy,
            currency,
        }
    }

    /// Reload the preimage for a settled payment, if the payment has settled.
    pub async fn reload_settled_preimage(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<Hash256>, LiquidityLoopOutError> {
        match self.reload_loop_out_payment_status(payment_hash).await? {
            LoopOutPaymentStatus::Settled(preimage) => Ok(Some(preimage)),
            LoopOutPaymentStatus::InFlight => Ok(None),
            LoopOutPaymentStatus::Failed(reason) => {
                Err(LiquidityLoopOutError::PaymentFailed(reason))
            }
        }
    }

    async fn reload_loop_out_payment_status(
        &self,
        payment_hash: Hash256,
    ) -> Result<LoopOutPaymentStatus, LiquidityLoopOutError> {
        let response = self.get_payment(payment_hash).await?;
        payment_status_from_response(response, payment_hash)
    }

    async fn get_payment(
        &self,
        payment_hash: Hash256,
    ) -> Result<SendPaymentResponse, LiquidityLoopOutError> {
        call!(self.network_actor, |reply| {
            NetworkActorMessage::Command(NetworkActorCommand::GetPayment(payment_hash, reply))
        })
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?
        .map_err(LiquidityLoopOutError::PaymentFailed)
    }

    async fn wait_for_settled_preimage(
        &self,
        payment_hash: Hash256,
        initial_response: SendPaymentResponse,
    ) -> Result<Hash256, LiquidityLoopOutError> {
        if let Some(preimage) = preimage_or_terminal_error(initial_response, payment_hash)? {
            return Ok(preimage);
        }

        for attempt in 0..self.polling_policy.max_reload_attempts {
            let response = self.get_payment(payment_hash).await?;
            if let Some(preimage) = preimage_or_terminal_error(response, payment_hash)? {
                return Ok(preimage);
            }

            if attempt + 1 < self.polling_policy.max_reload_attempts {
                tokio::time::sleep(self.polling_policy.reload_interval).await;
            }
        }

        Err(LiquidityLoopOutError::PaymentFailed(format!(
            "loop out payment did not settle before polling limit: {payment_hash:?}"
        )))
    }
}

#[async_trait::async_trait]
impl LoopOutPaymentAdapter for NetworkLoopOutPaymentAdapter {
    type Error = LiquidityLoopOutError;

    async fn send_loop_out_payment(
        &mut self,
        request: LoopOutPaymentRequest,
    ) -> Result<Hash256, Self::Error> {
        let payment_hash = request.payment_hash;
        let invoice = request.invoice.clone();
        let response = call!(self.network_actor, |reply| {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    target_pubkey: request.target_pubkey,
                    amount: Some(request.amount),
                    payment_hash: invoice.is_none().then_some(payment_hash),
                    invoice,
                    final_tlc_expiry_delta: None,
                    tlc_expiry_limit: None,
                    timeout: None,
                    max_fee_amount: Some(request.max_fee_amount),
                    max_fee_rate: None,
                    max_parts: None,
                    keysend: Some(false),
                    udt_type_script: None,
                    allow_self_payment: false,
                    custom_records: None,
                    hop_hints: None,
                    dry_run: false,
                    trampoline_hops: None,
                },
                reply,
            ))
        })
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?
        .map_err(LiquidityLoopOutError::PaymentFailed)?;

        let response_payment_hash = response.payment_hash;
        self.wait_for_settled_preimage(response_payment_hash, response)
            .await
    }

    async fn reload_loop_out_payment(
        &mut self,
        payment_hash: Hash256,
    ) -> Result<LoopOutPaymentStatus, Self::Error> {
        self.reload_loop_out_payment_status(payment_hash).await
    }

    async fn register_provider_loop_out_invoice(
        &mut self,
        payment_hash: Hash256,
        preimage: Hash256,
        amount: u128,
        udt_type_script: Option<ckb_types::packed::Script>,
    ) -> Result<(), Self::Error> {
        let mut builder = InvoiceBuilder::new(self.currency)
            .amount(Some(amount))
            .payment_preimage(preimage);
        if let Some(script) = udt_type_script {
            builder = builder.udt_type_script(script);
        }
        let invoice = builder
            .build()
            .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?;
        let invoice_hash = invoice.payment_hash();
        if *invoice_hash != payment_hash {
            return Err(LiquidityLoopOutError::PaymentFailed(format!(
                "provider invoice payment hash mismatch: expected {payment_hash:?}, got {invoice_hash:?}"
            )));
        }
        let result = call!(self.network_actor, |reply| {
            NetworkActorMessage::Command(NetworkActorCommand::AddInvoice(
                invoice.clone(),
                Some(preimage),
                reply,
            ))
        })
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?;
        match result {
            Ok(()) => Ok(()),
            Err(InvoiceError::InvoiceAlreadyExists) => Ok(()),
            Err(error) => Err(LiquidityLoopOutError::PaymentFailed(error.to_string())),
        }
    }

    async fn reload_provider_loop_out_payment(
        &mut self,
        payment_hash: Hash256,
    ) -> Result<LoopOutPaymentStatus, Self::Error> {
        let result = call!(self.network_actor, |reply| {
            NetworkActorMessage::Command(NetworkActorCommand::GetInvoice(payment_hash, reply))
        })
        .map_err(|error| LiquidityLoopOutError::PaymentFailed(error.to_string()))?;
        match result {
            Ok((_, status)) => match status {
                CkbInvoiceStatus::Paid => Ok(LoopOutPaymentStatus::Settled(payment_hash)),
                CkbInvoiceStatus::Open | CkbInvoiceStatus::Received => {
                    Ok(LoopOutPaymentStatus::InFlight)
                }
                CkbInvoiceStatus::Cancelled | CkbInvoiceStatus::Expired => {
                    Ok(LoopOutPaymentStatus::Failed(format!("invoice {status:?}")))
                }
            },
            Err(error) => Err(LiquidityLoopOutError::PaymentFailed(error.to_string())),
        }
    }
}

fn preimage_or_terminal_error(
    response: SendPaymentResponse,
    payment_hash: Hash256,
) -> Result<Option<Hash256>, LiquidityLoopOutError> {
    match response.status {
        PaymentStatus::Success => response
            .payment_preimage
            .ok_or_else(|| {
                LiquidityLoopOutError::PaymentFailed(
                    "settled payment is missing preimage".to_string(),
                )
            })
            .map(Some),
        PaymentStatus::Failed => Err(LiquidityLoopOutError::PaymentFailed(
            response.failed_error.unwrap_or_else(|| {
                format!("loop out payment failed without error detail: {payment_hash:?}")
            }),
        )),
        PaymentStatus::Created | PaymentStatus::Inflight => Ok(None),
    }
}

fn payment_status_from_response(
    response: SendPaymentResponse,
    payment_hash: Hash256,
) -> Result<LoopOutPaymentStatus, LiquidityLoopOutError> {
    match response.status {
        PaymentStatus::Success => response
            .payment_preimage
            .ok_or_else(|| {
                LiquidityLoopOutError::PaymentFailed(
                    "settled payment is missing preimage".to_string(),
                )
            })
            .map(LoopOutPaymentStatus::Settled),
        PaymentStatus::Failed => Ok(LoopOutPaymentStatus::Failed(
            response.failed_error.unwrap_or_else(|| {
                format!("loop out payment failed without error detail: {payment_hash:?}")
            }),
        )),
        PaymentStatus::Created | PaymentStatus::Inflight => Ok(LoopOutPaymentStatus::InFlight),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use fiber_types::{HashAlgorithm, PaymentStatus};
    use ractor::{Actor, ActorProcessingErr, ActorRef};
    use secp256k1::{SecretKey, SECP256K1};

    use crate::fiber::network::SendPaymentResponse;
    use crate::fiber::payment::SendPaymentCommand;
    use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
    use crate::invoice::{CkbInvoice, CkbInvoiceStatus};

    use super::*;

    #[test]
    fn payment_request_uses_principal_and_fee_cap() {
        let request =
            LoopOutPaymentRequest::new([1u8; 32].into(), test_pubkey(), 1_000, 1, 100).unwrap();

        assert_eq!(request.amount, 1_001);
        assert_eq!(request.max_fee_amount, 100);
        assert_eq!(request.target_pubkey, Some(test_pubkey()));
        assert_eq!(request.invoice, None);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_sends_existing_network_payment_command() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::Settle([7u8; 32].into())).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let preimage = adapter
            .send_loop_out_payment(
                LoopOutPaymentRequest::new([3u8; 32].into(), test_pubkey(), 100, 2, 5).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(preimage, [7u8; 32].into());
        assert_eq!(network.take_events(), vec!["send_payment"]);
        let command = network.take_send_commands().pop().unwrap();
        assert_eq!(command.payment_hash, Some([3u8; 32].into()));
        assert_eq!(command.amount, Some(102));
        assert_eq!(command.max_fee_amount, Some(5));
        assert_eq!(command.target_pubkey, Some(test_pubkey()));
        assert_eq!(command.invoice, None);
        assert_eq!(command.keysend, Some(false));
        assert_eq!(command.udt_type_script, None);
        assert!(!command.allow_self_payment);
        assert_eq!(command.custom_records, None);
        assert!(command.hop_hints.is_none());
        assert!(!command.dry_run);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_sends_invoice_payment_without_provider_target() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::Settle([7u8; 32].into())).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());
        let invoice = "lnbc-client-invoice".to_string();

        let preimage = adapter
            .send_loop_out_payment(LoopOutPaymentRequest::new_invoice(
                [3u8; 32].into(),
                invoice.clone(),
                100,
                0,
            ))
            .await
            .unwrap();

        assert_eq!(preimage, [7u8; 32].into());
        let command = network.take_send_commands().pop().unwrap();
        assert_eq!(command.invoice, Some(invoice));
        assert_eq!(command.target_pubkey, None);
        assert_eq!(command.payment_hash, None);
        assert_eq!(command.amount, Some(100));
        assert_eq!(command.max_fee_amount, Some(0));
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_polls_until_sent_payment_settles() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::SendInflightThenReloadSettled(
            [7u8; 32].into(),
        ))
        .await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let preimage = adapter
            .send_loop_out_payment(
                LoopOutPaymentRequest::new([3u8; 32].into(), test_pubkey(), 100, 2, 5).unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(preimage, [7u8; 32].into());
        assert_eq!(network.take_events(), vec!["send_payment", "get_payment"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_reloads_settled_payment() {
        let network =
            spawn_payment_mock(NetworkPaymentMockMode::ReloadSettled([8u8; 32].into())).await;
        let adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let preimage = adapter
            .reload_settled_preimage([3u8; 32].into())
            .await
            .unwrap();

        assert_eq!(preimage, Some([8u8; 32].into()));
        assert_eq!(network.take_events(), vec!["get_payment"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_reloads_non_settled_payment_as_none() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadInflight).await;
        let adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let preimage = adapter
            .reload_settled_preimage([3u8; 32].into())
            .await
            .unwrap();

        assert_eq!(preimage, None);
        assert_eq!(network.take_events(), vec!["get_payment"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_reloads_failed_payment_as_error() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadFailed).await;
        let adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let error = adapter
            .reload_settled_preimage([3u8; 32].into())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("route failed"));
        assert_eq!(network.take_events(), vec!["get_payment"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_classifies_failed_reload() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadFailed).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let status = adapter
            .reload_loop_out_payment([3u8; 32].into())
            .await
            .unwrap();

        assert_eq!(
            status,
            LoopOutPaymentStatus::Failed("route failed".to_string())
        );
        assert_eq!(network.take_events(), vec!["get_payment"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_errors_when_success_has_no_preimage() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::SettleWithoutPreimage).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let error = adapter
            .send_loop_out_payment(
                LoopOutPaymentRequest::new([3u8; 32].into(), test_pubkey(), 100, 2, 5).unwrap(),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing preimage"));
    }

    #[tokio::test]
    async fn register_provider_invoice_stores_invoice_and_preimage() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::Settle([0u8; 32].into())).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());
        let preimage: Hash256 = [9u8; 32].into();
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();

        adapter
            .register_provider_loop_out_invoice(payment_hash, preimage, 100, None)
            .await
            .unwrap();

        assert_eq!(network.take_events(), vec!["add_invoice"]);
        let invoices = network.take_invoices();
        assert_eq!(invoices.len(), 1);
        let (invoice, stored_preimage) = invoices.into_iter().next().unwrap();
        assert_eq!(*invoice.payment_hash(), payment_hash);
        assert_eq!(stored_preimage, Some(preimage));
    }

    #[tokio::test]
    async fn register_provider_invoice_is_idempotent_when_invoice_already_exists() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::Settle([0u8; 32].into())).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());
        let preimage: Hash256 = [9u8; 32].into();
        let payment_hash: Hash256 = HashAlgorithm::CkbHash.hash(preimage.as_ref()).into();

        adapter
            .register_provider_loop_out_invoice(payment_hash, preimage, 100, None)
            .await
            .unwrap();
        adapter
            .register_provider_loop_out_invoice(payment_hash, preimage, 100, None)
            .await
            .unwrap();

        assert_eq!(network.take_events(), vec!["add_invoice", "add_invoice"]);
        assert_eq!(network.take_invoices().len(), 1);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_reloads_provider_invoice_as_in_flight() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadInvoiceStatus(
            CkbInvoiceStatus::Open,
        ))
        .await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let status = adapter
            .reload_provider_loop_out_payment([3u8; 32].into())
            .await
            .unwrap();

        assert_eq!(status, LoopOutPaymentStatus::InFlight);
        assert_eq!(network.take_events(), vec!["get_invoice"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_reloads_provider_invoice_as_settled() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadInvoiceStatus(
            CkbInvoiceStatus::Paid,
        ))
        .await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let status = adapter
            .reload_provider_loop_out_payment([3u8; 32].into())
            .await
            .unwrap();

        assert_eq!(status, LoopOutPaymentStatus::Settled([3u8; 32].into()));
        assert_eq!(network.take_events(), vec!["get_invoice"]);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_reloads_provider_invoice_as_failed() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::ReloadInvoiceStatus(
            CkbInvoiceStatus::Cancelled,
        ))
        .await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let status = adapter
            .reload_provider_loop_out_payment([3u8; 32].into())
            .await
            .unwrap();

        assert_eq!(
            status,
            LoopOutPaymentStatus::Failed("invoice Cancelled".to_string())
        );
        assert_eq!(network.take_events(), vec!["get_invoice"]);
    }

    type MockInvoice = (CkbInvoice, Option<Hash256>);
    type MockInvoiceStore = Arc<Mutex<HashMap<Hash256, MockInvoice>>>;

    struct NetworkPaymentMock {
        actor: ActorRef<NetworkActorMessage>,
        events: Arc<Mutex<Vec<&'static str>>>,
        send_commands: Arc<Mutex<Vec<SendPaymentCommand>>>,
        invoices: MockInvoiceStore,
    }

    impl NetworkPaymentMock {
        fn take_events(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.events.lock().unwrap())
        }

        fn take_send_commands(&self) -> Vec<SendPaymentCommand> {
            std::mem::take(&mut self.send_commands.lock().unwrap())
        }

        fn take_invoices(&self) -> Vec<MockInvoice> {
            self.invoices
                .lock()
                .unwrap()
                .drain()
                .map(|(_, invoice)| invoice)
                .collect()
        }
    }

    enum NetworkPaymentMockMode {
        Settle(Hash256),
        SendInflightThenReloadSettled(Hash256),
        ReloadSettled(Hash256),
        ReloadInflight,
        ReloadFailed,
        SettleWithoutPreimage,
        ReloadInvoiceStatus(CkbInvoiceStatus),
    }

    struct NetworkPaymentMockActor;

    struct NetworkPaymentMockState {
        mode: NetworkPaymentMockMode,
        events: Arc<Mutex<Vec<&'static str>>>,
        send_commands: Arc<Mutex<Vec<SendPaymentCommand>>>,
        invoices: MockInvoiceStore,
    }

    struct NetworkPaymentMockArguments {
        mode: NetworkPaymentMockMode,
        events: Arc<Mutex<Vec<&'static str>>>,
        send_commands: Arc<Mutex<Vec<SendPaymentCommand>>>,
        invoices: MockInvoiceStore,
    }

    #[async_trait]
    impl Actor for NetworkPaymentMockActor {
        type Msg = NetworkActorMessage;
        type State = NetworkPaymentMockState;
        type Arguments = NetworkPaymentMockArguments;

        async fn pre_start(
            &self,
            _myself: ActorRef<Self::Msg>,
            args: Self::Arguments,
        ) -> Result<Self::State, ActorProcessingErr> {
            Ok(NetworkPaymentMockState {
                mode: args.mode,
                events: args.events,
                send_commands: args.send_commands,
                invoices: args.invoices,
            })
        }

        async fn handle(
            &self,
            _myself: ActorRef<Self::Msg>,
            message: Self::Msg,
            state: &mut Self::State,
        ) -> Result<(), ActorProcessingErr> {
            if let NetworkActorMessage::Command(command) = message {
                match command {
                    NetworkActorCommand::SendPayment(command, reply) => {
                        state.events.lock().unwrap().push("send_payment");
                        let payment_hash = command.payment_hash.unwrap_or_default();
                        state.send_commands.lock().unwrap().push(command);
                        let preimage = match state.mode {
                            NetworkPaymentMockMode::Settle(preimage) => Some(preimage),
                            NetworkPaymentMockMode::SettleWithoutPreimage => None,
                            NetworkPaymentMockMode::SendInflightThenReloadSettled(_) => {
                                let _ = reply.send(Ok(payment_response(
                                    payment_hash,
                                    PaymentStatus::Inflight,
                                    None,
                                )));
                                return Ok(());
                            }
                            _ => unreachable!("send payment mode must settle"),
                        };
                        let _ = reply.send(Ok(payment_response(
                            payment_hash,
                            PaymentStatus::Success,
                            preimage,
                        )));
                    }
                    NetworkActorCommand::GetPayment(payment_hash, reply) => {
                        state.events.lock().unwrap().push("get_payment");
                        let (status, preimage) = match state.mode {
                            NetworkPaymentMockMode::ReloadSettled(preimage) => {
                                (PaymentStatus::Success, Some(preimage))
                            }
                            NetworkPaymentMockMode::ReloadInflight => {
                                (PaymentStatus::Inflight, None)
                            }
                            NetworkPaymentMockMode::ReloadFailed => (PaymentStatus::Failed, None),
                            NetworkPaymentMockMode::SendInflightThenReloadSettled(preimage) => {
                                (PaymentStatus::Success, Some(preimage))
                            }
                            _ => unreachable!("get payment mode must reload"),
                        };
                        let _ = reply.send(Ok(payment_response(payment_hash, status, preimage)));
                    }
                    NetworkActorCommand::AddInvoice(invoice, preimage, reply) => {
                        state.events.lock().unwrap().push("add_invoice");
                        let payment_hash = *invoice.payment_hash();
                        let mut invoices = state.invoices.lock().unwrap();
                        let result = match invoices.entry(payment_hash) {
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                entry.insert((invoice, preimage));
                                Ok(())
                            }
                            std::collections::hash_map::Entry::Occupied(_) => {
                                Err(InvoiceError::InvoiceAlreadyExists)
                            }
                        };
                        let _ = reply.send(result);
                    }
                    NetworkActorCommand::GetInvoice(payment_hash, reply) => {
                        state.events.lock().unwrap().push("get_invoice");
                        let result = match state.mode {
                            NetworkPaymentMockMode::ReloadInvoiceStatus(status) => {
                                let invoice = InvoiceBuilder::new(Currency::Fibd)
                                    .payment_preimage([9u8; 32].into())
                                    .build()
                                    .expect("mock invoice");
                                Ok((invoice, status))
                            }
                            _ => state
                                .invoices
                                .lock()
                                .unwrap()
                                .get(&payment_hash)
                                .cloned()
                                .map(|(invoice, _)| (invoice, CkbInvoiceStatus::Open))
                                .ok_or(InvoiceError::InvoiceNotFound),
                        };
                        let _ = reply.send(result);
                    }
                    _ => unreachable!("unexpected network command"),
                }
            }
            Ok(())
        }
    }

    async fn spawn_payment_mock(mode: NetworkPaymentMockMode) -> NetworkPaymentMock {
        let events = Arc::new(Mutex::new(Vec::new()));
        let send_commands = Arc::new(Mutex::new(Vec::new()));
        let invoices = Arc::new(Mutex::new(HashMap::new()));
        let (actor, _handle) = ractor::Actor::spawn(
            None,
            NetworkPaymentMockActor,
            NetworkPaymentMockArguments {
                mode,
                events: events.clone(),
                send_commands: send_commands.clone(),
                invoices: invoices.clone(),
            },
        )
        .await
        .unwrap();
        NetworkPaymentMock {
            actor,
            events,
            send_commands,
            invoices,
        }
    }

    fn payment_response(
        payment_hash: Hash256,
        status: PaymentStatus,
        preimage: Option<Hash256>,
    ) -> SendPaymentResponse {
        SendPaymentResponse {
            payment_hash,
            status,
            created_at: 1,
            last_updated_at: 2,
            failed_error: (status == PaymentStatus::Failed).then(|| "route failed".to_string()),
            custom_records: None,
            fee: 0,
            payment_preimage: preimage,
            #[cfg(any(debug_assertions, test, feature = "bench"))]
            routers: vec![],
        }
    }

    fn test_pubkey() -> Pubkey {
        let sk = SecretKey::from_slice(&[42; 32]).unwrap();
        Pubkey::from(sk.public_key(SECP256K1))
    }
}
