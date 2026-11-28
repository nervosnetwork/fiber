use super::*;
use ractor::call_t;
use secp256k1::SecretKey;
use tempfile::tempdir;

struct TestCkbChainActor(SecretKey);

#[async_trait::async_trait]
impl Actor for TestCkbChainActor {
    type Msg = CkbChainMessage;
    type State = CkbChainState;
    type Arguments = CkbConfig;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        config: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        let signer = LocalSigner::new(self.0);
        let funding_source_lock_script =
            get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
        let ckb_tx_tracing_actor = Actor::spawn_linked(
            None,
            CkbTxTracingActor::new(),
            CkbTxTracingArguments {
                rpc_url: config.rpc_url.clone(),
                polling_interval: Duration::from_millis(config.tx_tracing_polling_interval_ms),
            },
            myself.into(),
        )
        .await?
        .0;
        Ok(CkbChainState {
            config,
            signer,
            funding_source_lock_script,
            ckb_tx_tracing_actor,
            live_cells_exclusion_map: Default::default(),
            local_funded_inputs_by_tx: Default::default(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        CkbChainActor {}.handle(myself, message, state).await
    }
}

#[tokio::test]
async fn test_build_unsigned_funding_tx_rejects_local_funding_lock() {
    let dir = tempdir().expect("create tempdir");
    let config = CkbConfig {
        base_dir: Some(dir.path().to_path_buf()),
        rpc_url: "http://127.0.0.1:8114".to_string(),
        tx_tracing_polling_interval_ms: 4_000,
        udt_whitelist: None,
        funding_tx_shell_builder: None,
    };
    let secret_key = SecretKey::from_slice(&[9u8; 32]).expect("valid secret key");
    let (actor, _handle) = Actor::spawn(None, TestCkbChainActor(secret_key), config)
        .await
        .expect("spawn TestCkbChainActor");

    let local_lock = call_t!(actor, CkbChainMessage::GetFundingSourceLockScript, 5_000)
        .expect("rpc call ok")
        .expect("local funding lock script present");

    let err = call_t!(
        actor,
        |reply| CkbChainMessage::BuildUnsignedFundingTx {
            funding_tx: FundingTx::new(),
            request: FundingRequest {
                script: local_lock.clone(),
                udt_type_script: None,
                local_amount: 100_000_000_000,
                funding_fee_rate: 1000,
                remote_amount: 0,
                local_reserved_ckb_amount: 0,
                remote_reserved_ckb_amount: 0,
            },
            funding_source_lock_script: local_lock.clone(),
            funding_source_lock_script_cell_deps: Vec::new(),
            funding_cell_lock_script: local_lock,
            reply,
        },
        5_000
    )
    .expect("rpc call ok")
    .expect_err("BuildUnsignedFundingTx must reject local funding lock");

    assert!(
        matches!(err, FundingError::ExternalFundingUsesLocalLock),
        "expected ExternalFundingUsesLocalLock, got {err:?}"
    );
}
