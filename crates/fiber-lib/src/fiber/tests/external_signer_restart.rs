use ckb_types::{
    core::FeeRate,
    packed::{Script, Transaction},
    prelude::{AsTransactionBuilder, Builder, Entity, Pack},
};
use fiber_json_types::{
    ChannelSigningStatus, ChannelSigningTransition, GetChannelSigningStatusParams,
    GetChannelSigningStatusResult, OpenChannelWithExternalFundingParams,
    OpenChannelWithExternalFundingResult, SubmitChannelSignatureParams,
    SubmitChannelSignatureResult, SubmitSignedFundingTxParams, SubmitSignedFundingTxResult,
};
use fiber_lsp_sdk::{
    ChannelKeyId, ChannelSignature, ChannelSigner, ChannelSigningContent, MemoryStore, RootKey,
    RootSigner,
};
use fiber_types::{ChannelState, Hash256, PaymentStatus, ShuttingDownFlags};

use crate::fiber::channel::{
    ChannelCommand, ChannelCommandWithId, ShutdownCommand, DEFAULT_COMMITMENT_FEE_RATE,
};
use crate::fiber::config::DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT;
use crate::fiber::network::{FiberActorCommand, NetworkActorMessage};
use crate::rpc::channel::to_rpc_channel_open_signer_material;
use crate::test_utils::{gen_rpc_config, init_tracing, NetworkNode, NetworkNodeConfigBuilder};
use crate::tests::test_utils::wait_until_async_timeout;
use ractor::call;
use std::sync::Mutex;
use std::time::Duration;

/// External signer with snapshot-based persistence and restart capability.
pub struct RestartableExternalSigner {
    root_key: [u8; 32],
    store: Mutex<MemoryStore>,
    channel_key_id: ChannelKeyId,
}

impl RestartableExternalSigner {
    pub async fn create() -> (Self, ChannelSigner<MemoryStore>) {
        let store = MemoryStore::default();
        let root_key = [42u8; 32];
        let root_signer = RootSigner::create(RootKey::import(root_key).unwrap(), store.clone())
            .await
            .unwrap();
        let signer = root_signer.create_channel().await.unwrap();
        let channel_key_id = signer.channel_key_id();
        (
            Self {
                root_key,
                store: Mutex::new(store),
                channel_key_id,
            },
            signer,
        )
    }

    pub async fn restart(&self) -> ChannelSigner<MemoryStore> {
        let snapshot = self.store.lock().unwrap().snapshot().unwrap();
        let restored_store = MemoryStore::from_snapshot(&snapshot).unwrap();
        let root_signer = RootSigner::open(
            RootKey::import(self.root_key).unwrap(),
            restored_store.clone(),
        )
        .await
        .unwrap();
        let signer = root_signer.open_channel(self.channel_key_id).await.unwrap();
        *self.store.lock().unwrap() = restored_store;
        signer
    }
}

fn mock_sign_external_funding_tx(unsigned_tx: &Transaction) -> Transaction {
    unsigned_tx
        .as_advanced_builder()
        .set_witnesses(vec![ckb_types::packed::Bytes::default()])
        .build()
        .data()
}

async fn bind_external_signer(
    signer: &ChannelSigner<MemoryStore>,
    unsigned_tx: &Transaction,
    shutdown_script: Script,
) {
    let expected_inputs: Vec<_> = unsigned_tx
        .raw()
        .inputs()
        .into_iter()
        .map(|input| input.previous_output())
        .collect();
    signer
        .bind_from_approved_funding(unsigned_tx, 0, shutdown_script, &expected_inputs)
        .await
        .expect("bind approved funding");
}

struct ExternalSignerHttpClient<'a> {
    node: &'a NetworkNode,
    signer: &'a ChannelSigner<MemoryStore>,
}

impl ExternalSignerHttpClient<'_> {
    async fn get_signing_status(&self, channel_id: Hash256) -> GetChannelSigningStatusResult {
        self.node
            .send_rpc_request(
                "get_channel_signing_status",
                GetChannelSigningStatusParams {
                    channel_id: channel_id.into(),
                },
            )
            .await
            .expect("get_channel_signing_status over HTTP")
    }

    async fn try_sign_pending(&self, channel_id: Hash256) {
        let status = self.get_signing_status(channel_id).await.status;
        if !matches!(status, ChannelSigningStatus::SignatureRequired { .. }) {
            return;
        }
        let submission = prepare_hosted_signature(self.signer, channel_id, status).await;
        let result = self
            .submit(submission)
            .await
            .expect("submit_channel_signature over HTTP");
        assert_eq!(result, SubmitChannelSignatureResult::Applied);
    }

    async fn submit(
        &self,
        params: SubmitChannelSignatureParams,
    ) -> Result<SubmitChannelSignatureResult, String> {
        self.node
            .send_rpc_request("submit_channel_signature", params)
            .await
    }
}

async fn prepare_hosted_signature(
    signer: &ChannelSigner<MemoryStore>,
    channel_id: Hash256,
    status: ChannelSigningStatus,
) -> SubmitChannelSignatureParams {
    let ChannelSigningStatus::SignatureRequired {
        request_id,
        content,
        ..
    } = status
    else {
        panic!("hosted channel must have a pending signing request");
    };
    let content =
        fiber_lsp_sdk::json::musig2_from_rpc(content).expect("hosted signing content must decode");
    let slot = content.slot;
    let prepared = signer
        .prepare(ChannelSigningContent::Musig2(content))
        .await
        .expect("prepare hosted external signature");
    let ChannelSignature::Musig2(signature) = signer
        .sign(prepared)
        .await
        .expect("sign hosted external request")
    else {
        panic!("hosted channel request must use MuSig2");
    };
    let next_material = signer
        .next_material(slot)
        .await
        .expect("load next hosted signer material");
    SubmitChannelSignatureParams {
        channel_id: channel_id.into(),
        request_id,
        partial_signature: signature.partial_signature.serialize(),
        next_material: Some(fiber_lsp_sdk::json::next_material_to_rpc(&next_material)),
    }
}

async fn wait_for_external_signer_recovery(
    node_a: &NetworkNode,
    node_b: &NetworkNode,
    signer: &ChannelSigner<MemoryStore>,
    channel_id: Hash256,
    payments: &[(&NetworkNode, Hash256)],
) -> bool {
    let sdk = ExternalSignerHttpClient {
        node: node_a,
        signer,
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            sdk.try_sign_pending(channel_id).await;
            let state_a = node_a.get_channel_actor_state(channel_id);
            let state_b = node_b.get_channel_actor_state(channel_id);
            let mut payments_settled = true;
            for (node, payment_hash) in payments {
                if node.get_payment_status(*payment_hash).await != PaymentStatus::Success {
                    payments_settled = false;
                    break;
                }
            }
            if payments_settled
                && matches!(state_a.state, ChannelState::ChannelReady)
                && matches!(state_b.state, ChannelState::ChannelReady)
                && !state_a.reestablishing
                && !state_b.reestablishing
                && state_a.tlc_state.all_tlcs().count() == 0
                && state_b.tlc_state.all_tlcs().count() == 0
                && !state_a.tlc_state.waiting_ack
                && !state_b.tlc_state.waiting_ack
                && !state_a.signing_context.is_awaiting_signature()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

/// Helper function to set up an external-signer channel.
pub async fn setup_restartable_external_channel(
    is_public: bool,
    signer: &ChannelSigner<MemoryStore>,
) -> ([NetworkNode; 2], Hash256) {
    let nodes = NetworkNode::new_n_interconnected_nodes_with_config(2, |index| {
        let mut builder = NetworkNodeConfigBuilder::new()
            .node_name(Some(format!("signer-restart-node-{index}")))
            .base_dir_prefix(&format!("signer-restart-node-{index}-"));
        if index == 0 {
            builder = builder.rpc_config(Some(gen_rpc_config()));
        } else {
            builder = builder.fiber_config_updater(|config| {
                config.auto_accept_channel_ckb_funding_amount =
                    Some(DEFAULT_AUTO_ACCEPT_CHANNEL_CKB_FUNDING_AMOUNT);
            });
        }
        builder.build()
    })
    .await;
    let [node_a, node_b]: [NetworkNode; 2] = nodes.try_into().unwrap();
    let material = signer.channel_open_material(is_public).await.unwrap();

    let open: OpenChannelWithExternalFundingResult = node_a
        .send_rpc_request(
            "open_channel_with_external_funding",
            OpenChannelWithExternalFundingParams {
                pubkey: node_b.pubkey.into(),
                funding_amount: 100_000_000_000,
                public: Some(is_public),
                funding_udt_type_script: None,
                shutdown_script: Script::default().into(),
                funding_lock_script: Script::default().into(),
                funding_lock_script_cell_deps: None,
                commitment_delay_epoch: None,
                commitment_fee_rate: None,
                funding_fee_rate: None,
                tlc_expiry_delta: None,
                tlc_min_value: None,
                tlc_fee_proportional_millionths: None,
                max_tlc_value_in_flight: None,
                max_tlc_number_in_flight: None,
                external_channel_signer: Some(to_rpc_channel_open_signer_material(&material)),
            },
        )
        .await
        .unwrap();

    let channel_id: Hash256 = open.channel_id.into();
    let unsigned_tx: Transaction = open.unsigned_funding_tx.into();
    bind_external_signer(signer, &unsigned_tx, Script::default()).await;
    let _: SubmitSignedFundingTxResult = node_a
        .send_rpc_request(
            "submit_signed_funding_tx",
            SubmitSignedFundingTxParams {
                channel_id: channel_id.into(),
                signed_funding_tx: mock_sign_external_funding_tx(&unsigned_tx).into(),
            },
        )
        .await
        .unwrap();

    let sdk = ExternalSignerHttpClient {
        node: &node_a,
        signer,
    };

    if is_public {
        // Wait until entering SignChannelAnnouncement state.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let status = sdk.get_signing_status(channel_id).await.status;
                if matches!(
                    status,
                    ChannelSigningStatus::SignatureRequired {
                        transition: ChannelSigningTransition::SignChannelAnnouncement,
                        ..
                    }
                ) {
                    break;
                }
                sdk.try_sign_pending(channel_id).await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("public channel pauses at SignChannelAnnouncement");
    } else {
        // Wait until channel reaches ChannelReady.
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let a_ready = matches!(
                    node_a.get_channel_actor_state(channel_id).state,
                    ChannelState::ChannelReady
                );
                let b_ready = node_b
                    .get_channel_actor_state_unchecked(channel_id)
                    .is_some_and(|state| matches!(state.state, ChannelState::ChannelReady));
                if a_ready && b_ready {
                    break;
                }
                sdk.try_sign_pending(channel_id).await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("channel reaches ChannelReady");

        // Seed payment from node_a to node_b to provide node_b with outbound liquidity for inbound tests.
        let seed_payment = node_a
            .send_payment_keysend(&node_b, 1_000_000, false)
            .await
            .expect("seed acceptor outbound liquidity");
        let settled = wait_for_external_signer_recovery(
            &node_a,
            &node_b,
            signer,
            channel_id,
            &[(&node_a, seed_payment.payment_hash)],
        )
        .await;
        assert!(settled, "seed payment settles");
    }

    ([node_a, node_b], channel_id)
}

/// Query channel external signing status.
pub async fn get_signing_status(
    node: &NetworkNode,
    channel_id: Hash256,
) -> Result<GetChannelSigningStatusResult, String> {
    node.send_rpc_request(
        "get_channel_signing_status",
        GetChannelSigningStatusParams {
            channel_id: channel_id.into(),
        },
    )
    .await
}

/// Submit an external channel signature to the node.
pub async fn submit_signature(
    node: &NetworkNode,
    params: SubmitChannelSignatureParams,
) -> Result<SubmitChannelSignatureResult, String> {
    node.send_rpc_request("submit_channel_signature", params)
        .await
}

/// Prepare and submit a signature for the specified SignatureRequired status.
pub async fn sign_and_submit(
    node: &NetworkNode,
    signer: &ChannelSigner<MemoryStore>,
    channel_id: Hash256,
    status: ChannelSigningStatus,
) -> SubmitChannelSignatureResult {
    let submission = prepare_hosted_signature(signer, channel_id, status).await;
    submit_signature(node, submission).await.unwrap()
}

// =========================================================================
// Suite A: Tenant Process Restart Tests
// =========================================================================

/// A1: Tenant restarts while paused awaiting SendCommitmentSigned signature.
#[tokio::test]
async fn test_tenant_restart_send_commitment_signed() {
    init_tracing();
    let (_restartable, signer) = RestartableExternalSigner::create().await;
    let ([mut tenant, mut public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    // 1. Start payment to trigger SendCommitmentSigned.
    let payment = tenant
        .send_payment_keysend(&public_node, 10_000, false)
        .await
        .expect("start keysend");

    // 2. State checkpoint: wait until entering SendCommitmentSigned.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendCommitmentSigned,
                ..
            })
        )
    })
    .await;
    let pre_req = match get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status
    {
        ChannelSigningStatus::SignatureRequired { request_id, .. } => request_id,
        _ => unreachable!(),
    };

    // 3. Action: Tenant crashes and restarts.
    tenant.restart().await;

    // 4. Assert persisted state is recovered after restart.
    let post_status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    match post_status {
        ChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            ..
        } => {
            assert_eq!(
                request_id, pre_req,
                "request_id must survive tenant restart"
            );
            assert_eq!(transition, ChannelSigningTransition::SendCommitmentSigned);
        }
        _ => panic!("tenant failed to restore SignatureRequired state"),
    }

    tenant.connect_to(&mut public_node).await;

    // 5. Submit signature to resume and verify settlement.
    let applied = sign_and_submit(&tenant, &signer, channel_id, post_status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &signer,
        channel_id,
        &[(&tenant, payment.payment_hash)],
    )
    .await;
    assert!(recovered, "payment settles after tenant restart");
}

/// A2: Tenant restarts while paused awaiting CompleteReceivedCommitment signature.
#[tokio::test]
async fn test_tenant_restart_complete_received_commitment() {
    init_tracing();
    let (_restartable, signer) = RestartableExternalSigner::create().await;
    let ([mut tenant, mut public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    // 1. Peer starts payment to trigger CompleteReceivedCommitment.
    let payment = public_node
        .send_payment_keysend(&tenant, 10_000, false)
        .await
        .expect("start inbound keysend");

    // 2. State checkpoint: wait until entering CompleteReceivedCommitment.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedCommitment,
                ..
            })
        )
    })
    .await;
    let pre_req = match get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status
    {
        ChannelSigningStatus::SignatureRequired { request_id, .. } => request_id,
        _ => unreachable!(),
    };

    // 3. Action: Tenant crashes and restarts.
    tenant.restart().await;

    // 4. Assert state is recovered.
    let post_status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    match post_status {
        ChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            ..
        } => {
            assert_eq!(request_id, pre_req);
            assert_eq!(
                transition,
                ChannelSigningTransition::CompleteReceivedCommitment
            );
        }
        _ => panic!("tenant failed to restore CompleteReceivedCommitment"),
    }

    tenant.connect_to(&mut public_node).await;

    // 5. Submit signature and complete revocation exchange.
    let applied = sign_and_submit(&tenant, &signer, channel_id, post_status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &signer,
        channel_id,
        &[(&public_node, payment.payment_hash)],
    )
    .await;
    assert!(recovered, "inbound payment settles after tenant restart");
}

/// A3: Tenant restarts while paused awaiting SendRevokeAndAck signature.
#[tokio::test]
async fn test_tenant_restart_send_revoke_and_ack() {
    init_tracing();
    let (_restartable, signer) = RestartableExternalSigner::create().await;
    let ([mut tenant, mut public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    let payment = public_node
        .send_payment_keysend(&tenant, 10_000, false)
        .await
        .expect("start inbound keysend");

    // Advance through CompleteReceivedCommitment and pause at SendRevokeAndAck.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedCommitment,
                ..
            })
        )
    })
    .await;
    let status_1 = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    sign_and_submit(&tenant, &signer, channel_id, status_1).await;

    // State checkpoint: wait until entering SendRevokeAndAck.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendRevokeAndAck,
                ..
            })
        )
    })
    .await;
    let pre_req = match get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status
    {
        ChannelSigningStatus::SignatureRequired { request_id, .. } => request_id,
        _ => unreachable!(),
    };

    // Action: Tenant crashes and restarts.
    tenant.restart().await;

    let post_status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    match post_status {
        ChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            ..
        } => {
            assert_eq!(request_id, pre_req);
            assert_eq!(transition, ChannelSigningTransition::SendRevokeAndAck);
        }
        _ => panic!("tenant failed to restore SendRevokeAndAck"),
    }

    tenant.connect_to(&mut public_node).await;

    let applied = sign_and_submit(&tenant, &signer, channel_id, post_status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &signer,
        channel_id,
        &[(&public_node, payment.payment_hash)],
    )
    .await;
    assert!(recovered, "channel completes revocation after restart");
}

/// A4: Tenant restarts while paused awaiting CompleteReceivedRevokeAndAck signature.
#[tokio::test]
async fn test_tenant_restart_complete_received_revoke_and_ack() {
    init_tracing();
    let (_restartable, signer) = RestartableExternalSigner::create().await;
    let ([mut tenant, mut public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    // Start outbound payment and submit initial commitment signature, awaiting peer's RevokeAndAck.
    let payment = tenant
        .send_payment_keysend(&public_node, 10_000, false)
        .await
        .expect("start keysend");

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendCommitmentSigned,
                ..
            })
        )
    })
    .await;
    let status_1 = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    sign_and_submit(&tenant, &signer, channel_id, status_1).await;

    // State checkpoint: peer sends RevokeAndAck, local enters CompleteReceivedRevokeAndAck.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedRevokeAndAck,
                ..
            })
        )
    })
    .await;
    let pre_req = match get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status
    {
        ChannelSigningStatus::SignatureRequired { request_id, .. } => request_id,
        _ => unreachable!(),
    };

    tenant.restart().await;

    let post_status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    match post_status {
        ChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            ..
        } => {
            assert_eq!(request_id, pre_req);
            assert_eq!(
                transition,
                ChannelSigningTransition::CompleteReceivedRevokeAndAck
            );
        }
        _ => panic!("tenant failed to restore CompleteReceivedRevokeAndAck"),
    }

    tenant.connect_to(&mut public_node).await;

    let applied = sign_and_submit(&tenant, &signer, channel_id, post_status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &signer,
        channel_id,
        &[(&tenant, payment.payment_hash)],
    )
    .await;
    assert!(
        recovered,
        "settlement succeeds after CompleteReceivedRevokeAndAck restart"
    );
}

/// A5: Tenant restarts while paused awaiting SendClosingSigned signature.
#[tokio::test]
async fn test_tenant_restart_send_closing_signed() {
    init_tracing();
    let (_restartable, signer) = RestartableExternalSigner::create().await;
    let ([mut tenant, mut public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    // Initiate cooperative channel shutdown.
    let message = |rpc_reply| {
        NetworkActorMessage::new_command(FiberActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Some(Script::new_builder().args([0u8; 19].pack()).build()),
                        fee_rate: Some(FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE)),
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    };
    call!(public_node.network_actor, message).unwrap().unwrap();

    // State checkpoint: wait until entering SendClosingSigned.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendClosingSigned,
                ..
            })
        )
    })
    .await;
    let pre_req = match get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status
    {
        ChannelSigningStatus::SignatureRequired { request_id, .. } => request_id,
        _ => unreachable!(),
    };

    tenant.restart().await;

    let post_status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    match post_status {
        ChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            ..
        } => {
            assert_eq!(request_id, pre_req);
            assert_eq!(transition, ChannelSigningTransition::SendClosingSigned);
        }
        _ => panic!("tenant failed to restore SendClosingSigned"),
    }

    tenant.connect_to(&mut public_node).await;

    let applied = sign_and_submit(&tenant, &signer, channel_id, post_status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    wait_until_async_timeout(|| async {
        let state = tenant.get_channel_actor_state(channel_id);
        matches!(
            state.state,
            ChannelState::ShuttingDown(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
                | ChannelState::Closed(_)
        )
    })
    .await;
}

/// A6: Tenant restarts while paused awaiting SignChannelAnnouncement signature.
#[tokio::test]
async fn test_tenant_restart_sign_channel_announcement() {
    init_tracing();
    let (_restartable, signer) = RestartableExternalSigner::create().await;
    // Establish a public channel.
    let ([mut tenant, mut public_node], channel_id) =
        setup_restartable_external_channel(true, &signer).await;

    // State checkpoint: wait until entering SignChannelAnnouncement.
    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SignChannelAnnouncement,
                ..
            })
        )
    })
    .await;
    let pre_req = match get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status
    {
        ChannelSigningStatus::SignatureRequired { request_id, .. } => request_id,
        _ => unreachable!(),
    };

    tenant.restart().await;

    let post_status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    match post_status {
        ChannelSigningStatus::SignatureRequired {
            request_id,
            transition,
            ..
        } => {
            assert_eq!(request_id, pre_req);
            assert_eq!(
                transition,
                ChannelSigningTransition::SignChannelAnnouncement
            );
        }
        _ => panic!("tenant failed to restore SignChannelAnnouncement"),
    }

    tenant.connect_to(&mut public_node).await;

    let applied = sign_and_submit(&tenant, &signer, channel_id, post_status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    wait_until_async_timeout(|| async {
        let state = tenant.get_channel_actor_state(channel_id);
        matches!(state.state, ChannelState::ChannelReady)
    })
    .await;
}

// =========================================================================
// Suite B: External Signer Crash & Restart Tests
// =========================================================================

/// B1: Signer crashes and restarts during SendCommitmentSigned.
#[tokio::test]
async fn test_signer_restart_send_commitment_signed() {
    init_tracing();
    let (restartable, signer) = RestartableExternalSigner::create().await;
    let ([tenant, public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    let payment = tenant
        .send_payment_keysend(&public_node, 10_000, false)
        .await
        .expect("start keysend");

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendCommitmentSigned,
                ..
            })
        )
    })
    .await;

    // Action: external signer service crashes.
    drop(signer);

    // Restart signer service from persistent snapshot.
    let restored_signer = restartable.restart().await;

    let status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    let applied = sign_and_submit(&tenant, &restored_signer, channel_id, status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &restored_signer,
        channel_id,
        &[(&tenant, payment.payment_hash)],
    )
    .await;
    assert!(recovered, "payment completes with restored signer");
}

/// B2: Signer crashes and restarts during CompleteReceivedCommitment.
#[tokio::test]
async fn test_signer_restart_complete_received_commitment() {
    init_tracing();
    let (restartable, signer) = RestartableExternalSigner::create().await;
    let ([tenant, public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    let payment = public_node
        .send_payment_keysend(&tenant, 10_000, false)
        .await
        .expect("start inbound keysend");

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedCommitment,
                ..
            })
        )
    })
    .await;

    drop(signer);
    let restored_signer = restartable.restart().await;

    let status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    let applied = sign_and_submit(&tenant, &restored_signer, channel_id, status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &restored_signer,
        channel_id,
        &[(&public_node, payment.payment_hash)],
    )
    .await;
    assert!(recovered, "inbound payment completes with restored signer");
}

/// B3: Signer crashes and restarts during SendRevokeAndAck.
#[tokio::test]
async fn test_signer_restart_send_revoke_and_ack() {
    init_tracing();
    let (restartable, signer) = RestartableExternalSigner::create().await;
    let ([tenant, public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    let payment = public_node
        .send_payment_keysend(&tenant, 10_000, false)
        .await
        .expect("start inbound keysend");

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedCommitment,
                ..
            })
        )
    })
    .await;
    let s1 = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    sign_and_submit(&tenant, &signer, channel_id, s1).await;

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendRevokeAndAck,
                ..
            })
        )
    })
    .await;

    drop(signer);
    let restored_signer = restartable.restart().await;

    let status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    let applied = sign_and_submit(&tenant, &restored_signer, channel_id, status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &restored_signer,
        channel_id,
        &[(&public_node, payment.payment_hash)],
    )
    .await;
    assert!(recovered, "revocation completes with restored signer");
}

/// B4: Signer crashes and restarts during CompleteReceivedRevokeAndAck.
#[tokio::test]
async fn test_signer_restart_complete_received_revoke_and_ack() {
    init_tracing();
    let (restartable, signer) = RestartableExternalSigner::create().await;
    let ([tenant, public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    let payment = tenant
        .send_payment_keysend(&public_node, 10_000, false)
        .await
        .expect("start keysend");

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendCommitmentSigned,
                ..
            })
        )
    })
    .await;
    let s1 = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    sign_and_submit(&tenant, &signer, channel_id, s1).await;

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::CompleteReceivedRevokeAndAck,
                ..
            })
        )
    })
    .await;

    drop(signer);
    let restored_signer = restartable.restart().await;

    let status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    let applied = sign_and_submit(&tenant, &restored_signer, channel_id, status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    let recovered = wait_for_external_signer_recovery(
        &tenant,
        &public_node,
        &restored_signer,
        channel_id,
        &[(&tenant, payment.payment_hash)],
    )
    .await;
    assert!(
        recovered,
        "CompleteReceivedRevokeAndAck succeeds with restored signer"
    );
}

/// B5: Signer crashes and restarts during SendClosingSigned.
#[tokio::test]
async fn test_signer_restart_send_closing_signed() {
    init_tracing();
    let (restartable, signer) = RestartableExternalSigner::create().await;
    let ([tenant, public_node], channel_id) =
        setup_restartable_external_channel(false, &signer).await;

    let message = |rpc_reply| {
        NetworkActorMessage::new_command(FiberActorCommand::ControlFiberChannel(
            ChannelCommandWithId {
                channel_id,
                command: ChannelCommand::Shutdown(
                    ShutdownCommand {
                        close_script: Some(Script::new_builder().args([0u8; 19].pack()).build()),
                        fee_rate: Some(FeeRate::from_u64(DEFAULT_COMMITMENT_FEE_RATE)),
                        force: false,
                    },
                    rpc_reply,
                ),
            },
        ))
    };
    call!(public_node.network_actor, message).unwrap().unwrap();

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SendClosingSigned,
                ..
            })
        )
    })
    .await;

    drop(signer);
    let restored_signer = restartable.restart().await;

    let status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    let applied = sign_and_submit(&tenant, &restored_signer, channel_id, status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    wait_until_async_timeout(|| async {
        let state = tenant.get_channel_actor_state(channel_id);
        matches!(
            state.state,
            ChannelState::ShuttingDown(ShuttingDownFlags::WAITING_COMMITMENT_CONFIRMATION)
                | ChannelState::Closed(_)
        )
    })
    .await;
}

/// B6: Signer crashes and restarts during SignChannelAnnouncement.
#[tokio::test]
async fn test_signer_restart_sign_channel_announcement() {
    init_tracing();
    let (restartable, signer) = RestartableExternalSigner::create().await;
    let ([tenant, _public_node], channel_id) =
        setup_restartable_external_channel(true, &signer).await;

    wait_until_async_timeout(|| async {
        matches!(
            get_signing_status(&tenant, channel_id)
                .await
                .map(|r| r.status),
            Ok(ChannelSigningStatus::SignatureRequired {
                transition: ChannelSigningTransition::SignChannelAnnouncement,
                ..
            })
        )
    })
    .await;

    drop(signer);
    let restored_signer = restartable.restart().await;

    let status = get_signing_status(&tenant, channel_id)
        .await
        .unwrap()
        .status;
    let applied = sign_and_submit(&tenant, &restored_signer, channel_id, status).await;
    assert_eq!(applied, SubmitChannelSignatureResult::Applied);

    wait_until_async_timeout(|| async {
        let state = tenant.get_channel_actor_state(channel_id);
        matches!(state.state, ChannelState::ChannelReady)
    })
    .await;
}
