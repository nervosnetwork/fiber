/// # How to Use This Example
///
/// Start CKB dev net first. Set the test account as the miner.
///
///
/// ```text
/// ckb init -c dev --force --ba-arg 0xc8328aabcd9b9e8e64fbc566c4385c3bdeb219d7
/// ```
///
/// Transfer some CKB to the address
///
/// ```text
/// ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqgx5lf4pczpamsfam48evs0c8nvwqqa59qapt46f`
/// ```
///
/// Run the example:
///
/// ```text
/// cargo run --example funding /tmp/ckb-local /tmp/ckb-remote
/// ```
use ckb_pcn_node::{
    ckb::types::Hash256,
    ckb_chain::{
        CkbChainActor, CkbChainConfig, CkbChainMessage, FundingRequest, FundingTx, TraceTxRequest,
    },
};
use ckb_types::packed;
use ractor::{call_t, cast, Actor, ActorRef};
use std::{env, path::PathBuf};

#[tokio::main]
pub async fn main() {
    env_logger::init();

    let (local_config, remote_config) = prepare();

    let (local_actor, local_handle) = Actor::spawn(
        Some("local actor".to_string()),
        CkbChainActor {},
        local_config,
    )
    .await
    .expect("start local actor");

    let (remote_actor, remote_handle) = Actor::spawn(
        Some("remote actor".to_string()),
        CkbChainActor {},
        remote_config,
    )
    .await
    .expect("start remote actor");

    run(&local_actor, &remote_actor).await;

    local_actor.stop(None);
    local_handle.await.expect("Actor failed to exit cleanly");
    remote_actor.stop(None);
    remote_handle.await.expect("Actor failed to exit cleanly");
}

fn prepare() -> (CkbChainConfig, CkbChainConfig) {
    let args: Vec<String> = env::args().collect();
    let local_path = PathBuf::from(&args[1]);
    let remote_path = PathBuf::from(&args[2]);
    let _ = std::fs::create_dir_all(&local_path);
    let _ = std::fs::create_dir_all(&remote_path);

    // Dev net test account that has 20 billions of CKB tokens in the genesis block.
    std::fs::write(
        local_path.join("key"),
        "d00c06bfd800d27397002dca6fb0993d5ba6399b4238b2f29ee9deb97593d2bc",
    )
    .expect("Unable to write to file");
    // privkey for ckt1qzda0cr08m85hc8jlnfp3zer7xulejywt49kt2rr0vthywaa50xwsqgx5lf4pczpamsfam48evs0c8nvwqqa59qapt46f
    std::fs::write(
        remote_path.join("key"),
        "cccd5f7e693b60447623fb71a5983f15a426938c33699b1a81d1239cfa656cd1",
    )
    .expect("Unable to write to file");

    let rpc_url = "http://127.0.0.1:8114".to_string();
    return (
        CkbChainConfig {
            base_dir: Some(local_path),
            rpc_url: rpc_url.clone(),
            funding_source_lock_script_code_hash: make_h256(
                "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
            ),
            funding_source_lock_script_hash_type: ckb_jsonrpc_types::ScriptHashType::Type,
            funding_cell_lock_script_code_hash: make_h256(
                "0x8090ce20be9976e2407511502acebf74ac1cfed10d7b35b7f33f56c9bd0daec6",
            ),
            funding_cell_lock_script_hash_type: ckb_jsonrpc_types::ScriptHashType::Type,
        },
        CkbChainConfig {
            base_dir: Some(remote_path),
            rpc_url: rpc_url.clone(),
            funding_source_lock_script_code_hash: make_h256(
                "0x9bd7e06f3ecf4be0f2fcd2188b23f1b9fcc88e5d4b65a8637b17723bbda3cce8",
            ),
            funding_source_lock_script_hash_type: ckb_jsonrpc_types::ScriptHashType::Type,
            funding_cell_lock_script_code_hash: make_h256(
                "0x8090ce20be9976e2407511502acebf74ac1cfed10d7b35b7f33f56c9bd0daec6",
            ),
            funding_cell_lock_script_hash_type: ckb_jsonrpc_types::ScriptHashType::Type,
        },
    );
}

const TIMEOUT_MS: u64 = 60000;

async fn run(local: &ActorRef<CkbChainMessage>, remote: &ActorRef<CkbChainMessage>) {
    let mut tx = FundingTx::new();
    let funding_cell_lock_script_args = packed::Bytes::default();

    // The only party to fund goes first. Or the one who does not pay fee goes first for dual funding.
    tx = call_t!(
        local,
        CkbChainMessage::Fund,
        TIMEOUT_MS,
        tx,
        FundingRequest {
            udt_info: None,
            funding_cell_lock_script_args: funding_cell_lock_script_args.clone(),
            local_amount: 12000000000,
            local_fee_rate: 0,
            remote_amount: 10000000000,
        }
    )
    .expect("local calls")
    .expect("local funds");
    log_tx("local funded", &tx);

    // The one who pays fee goes next.
    tx = call_t!(
        remote,
        CkbChainMessage::Fund,
        TIMEOUT_MS,
        tx,
        FundingRequest {
            udt_info: None,
            funding_cell_lock_script_args: funding_cell_lock_script_args.clone(),
            local_amount: 10000000000,
            local_fee_rate: 1000,
            remote_amount: 12000000000,
        }
    )
    .expect("remote calls")
    .expect("remote funds");
    log_tx("remote funded", &tx);

    // Now tx is ready and the funding cell out point is available. Exchange and confirm commitment tx.

    // Exchange funding tx signatures once commitment txs are confirmed.
    tx = call_t!(local, CkbChainMessage::Sign, TIMEOUT_MS, tx)
        .expect("local calls")
        .expect("local signs");
    log_tx("local signed", &tx);
    tx = call_t!(remote, CkbChainMessage::Sign, TIMEOUT_MS, tx)
        .expect("remote calls")
        .expect("remote signs");
    log_tx("remote signed", &tx);

    let tx = tx.take().expect("take tx");
    // Broadcast funding tx and waiting for confs
    cast!(local, CkbChainMessage::SendTx(tx.clone())).expect("local sends");
    log::info!("tx sent: {}", tx.hash());

    let status = call_t!(
        local,
        CkbChainMessage::TraceTx,
        TIMEOUT_MS,
        TraceTxRequest {
            tx_hash: tx.hash(),
            confirmations: 3
        }
    )
    .expect("remote calls");

    log::info!("tx status of {}: {:?}", tx.hash(), status);
}

fn make_h256(hex: &str) -> Hash256 {
    let mut buf = [0u8; 32];
    hex::decode_to_slice(&hex[2..], &mut buf).expect("decode hash256");
    Hash256::from(buf)
}

fn log_tx(name: &str, tx: &FundingTx) {
    match tx.as_ref() {
        Some(tx) => {
            let tx: ckb_jsonrpc_types::TransactionView = tx.clone().into();
            log::info!(
                "tx[{}]: {}",
                name,
                serde_json::to_string_pretty(&tx).expect("serde json")
            )
        }
        None => log::info!("tx[{}]: None", name),
    }
}
