//! Payment request models for client-side Loop Out execution.

use std::time::Duration;

use fiber_types::{Hash256, PaymentStatus};
use ractor::{call, ActorRef};

use crate::fiber::network::SendPaymentResponse;
use crate::fiber::payment::SendPaymentCommand;
use crate::fiber::{NetworkActorCommand, NetworkActorMessage};
use crate::liquidity::actor::LoopOutPaymentAdapter;
use crate::liquidity::types::{loop_out_gross_payment_amount, LiquidityLoopOutError};

/// Fiber payment request derived from accepted Loop Out terms.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LoopOutPaymentRequest {
    /// Payment hash identifying the HTLC/conditional payment.
    pub payment_hash: Hash256,
    /// Gross Fiber payment amount including provider and routing fee budgets.
    pub amount: u128,
    /// Maximum Fiber routing fee the client accepts for this payment.
    pub max_fee_amount: u128,
}

impl LoopOutPaymentRequest {
    /// Build a Loop Out payment request from the net on-chain amount and fee limits.
    pub fn new(
        payment_hash: Hash256,
        amount: u128,
        provider_fee: u128,
        routing_fee_limit: u128,
    ) -> Result<Self, LiquidityLoopOutError> {
        Ok(Self {
            payment_hash,
            amount: loop_out_gross_payment_amount(amount, provider_fee, routing_fee_limit)?,
            max_fee_amount: routing_fee_limit,
        })
    }
}

/// Payment adapter that sends Loop Out payments through the existing Fiber network actor.
#[derive(Clone)]
pub struct NetworkLoopOutPaymentAdapter {
    network_actor: ActorRef<NetworkActorMessage>,
    polling_policy: NetworkLoopOutPaymentPollingPolicy,
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
        Self::with_polling_policy(network_actor, NetworkLoopOutPaymentPollingPolicy::default())
    }

    /// Create a network-backed Loop Out payment adapter with a custom polling policy.
    pub fn with_polling_policy(
        network_actor: ActorRef<NetworkActorMessage>,
        polling_policy: NetworkLoopOutPaymentPollingPolicy,
    ) -> Self {
        Self {
            network_actor,
            polling_policy,
        }
    }

    /// Reload the preimage for a settled payment, if the payment has settled.
    pub async fn reload_settled_preimage(
        &self,
        payment_hash: Hash256,
    ) -> Result<Option<Hash256>, LiquidityLoopOutError> {
        let response = self.get_payment(payment_hash).await?;

        settled_preimage_from_response(response)
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
        let response = call!(self.network_actor, |reply| {
            NetworkActorMessage::Command(NetworkActorCommand::SendPayment(
                SendPaymentCommand {
                    target_pubkey: None,
                    amount: Some(request.amount),
                    payment_hash: Some(payment_hash),
                    invoice: None,
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

        self.wait_for_settled_preimage(payment_hash, response).await
    }
}

fn preimage_or_terminal_error(
    response: SendPaymentResponse,
    payment_hash: Hash256,
) -> Result<Option<Hash256>, LiquidityLoopOutError> {
    match response.status {
        PaymentStatus::Success => response
            .preimage
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

fn settled_preimage_from_response(
    response: SendPaymentResponse,
) -> Result<Option<Hash256>, LiquidityLoopOutError> {
    if response.status == PaymentStatus::Success {
        return response
            .preimage
            .ok_or_else(|| {
                LiquidityLoopOutError::PaymentFailed(
                    "settled payment is missing preimage".to_string(),
                )
            })
            .map(Some);
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use fiber_types::PaymentStatus;
    use ractor::{Actor, ActorProcessingErr, ActorRef};

    use crate::fiber::network::SendPaymentResponse;
    use crate::fiber::payment::SendPaymentCommand;
    use crate::fiber::{NetworkActorCommand, NetworkActorMessage};

    use super::*;

    #[test]
    fn payment_request_uses_gross_amount_and_fee_cap() {
        let request = LoopOutPaymentRequest::new([1u8; 32].into(), 100, 2, 3).unwrap();

        assert_eq!(request.amount, 105);
        assert_eq!(request.max_fee_amount, 3);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_sends_existing_network_payment_command() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::Settle([7u8; 32].into())).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let preimage = adapter
            .send_loop_out_payment(LoopOutPaymentRequest::new([3u8; 32].into(), 100, 2, 5).unwrap())
            .await
            .unwrap();

        assert_eq!(preimage, [7u8; 32].into());
        assert_eq!(network.take_events(), vec!["send_payment"]);
        let command = network.take_send_commands().pop().unwrap();
        assert_eq!(command.payment_hash, Some([3u8; 32].into()));
        assert_eq!(command.amount, Some(107));
        assert_eq!(command.max_fee_amount, Some(5));
        assert_eq!(command.target_pubkey, None);
        assert_eq!(command.invoice, None);
        assert_eq!(command.keysend, Some(false));
        assert_eq!(command.udt_type_script, None);
        assert!(!command.allow_self_payment);
        assert_eq!(command.custom_records, None);
        assert!(command.hop_hints.is_none());
        assert!(!command.dry_run);
    }

    #[tokio::test]
    async fn network_loop_out_payment_adapter_polls_until_sent_payment_settles() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::SendInflightThenReloadSettled(
            [7u8; 32].into(),
        ))
        .await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let preimage = adapter
            .send_loop_out_payment(LoopOutPaymentRequest::new([3u8; 32].into(), 100, 2, 5).unwrap())
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
    async fn network_loop_out_payment_adapter_errors_when_success_has_no_preimage() {
        let network = spawn_payment_mock(NetworkPaymentMockMode::SettleWithoutPreimage).await;
        let mut adapter = NetworkLoopOutPaymentAdapter::new(network.actor.clone());

        let error = adapter
            .send_loop_out_payment(LoopOutPaymentRequest::new([3u8; 32].into(), 100, 2, 5).unwrap())
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing preimage"));
    }

    struct NetworkPaymentMock {
        actor: ActorRef<NetworkActorMessage>,
        events: Arc<Mutex<Vec<&'static str>>>,
        send_commands: Arc<Mutex<Vec<SendPaymentCommand>>>,
    }

    impl NetworkPaymentMock {
        fn take_events(&self) -> Vec<&'static str> {
            std::mem::take(&mut self.events.lock().unwrap())
        }

        fn take_send_commands(&self) -> Vec<SendPaymentCommand> {
            std::mem::take(&mut self.send_commands.lock().unwrap())
        }
    }

    enum NetworkPaymentMockMode {
        Settle(Hash256),
        SendInflightThenReloadSettled(Hash256),
        ReloadSettled(Hash256),
        ReloadInflight,
        SettleWithoutPreimage,
    }

    struct NetworkPaymentMockActor;

    struct NetworkPaymentMockState {
        mode: NetworkPaymentMockMode,
        events: Arc<Mutex<Vec<&'static str>>>,
        send_commands: Arc<Mutex<Vec<SendPaymentCommand>>>,
    }

    struct NetworkPaymentMockArguments {
        mode: NetworkPaymentMockMode,
        events: Arc<Mutex<Vec<&'static str>>>,
        send_commands: Arc<Mutex<Vec<SendPaymentCommand>>>,
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
                            NetworkPaymentMockMode::SendInflightThenReloadSettled(preimage) => {
                                (PaymentStatus::Success, Some(preimage))
                            }
                            _ => unreachable!("get payment mode must reload"),
                        };
                        let _ = reply.send(Ok(payment_response(payment_hash, status, preimage)));
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
        let (actor, _handle) = ractor::Actor::spawn(
            None,
            NetworkPaymentMockActor,
            NetworkPaymentMockArguments {
                mode,
                events: events.clone(),
                send_commands: send_commands.clone(),
            },
        )
        .await
        .unwrap();
        NetworkPaymentMock {
            actor,
            events,
            send_commands,
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
            failed_error: None,
            custom_records: None,
            fee: 0,
            preimage,
            #[cfg(any(debug_assertions, test, feature = "bench"))]
            routers: vec![],
        }
    }
}
