use lnd::bitcoind::{self, BitcoinD};
use lnd::{self, Lnd, LndConf};
use tracing::debug;

fn get_executables_paths() -> (String, String) {
    let bitcoind_exe_path = bitcoind::exe_path()
        .or(bitcoind::downloaded_exe_path())
        .expect("bitcoind executable either exists locally or can be downloaded");
    let lnd_exe_path = lnd::exe_path()
        .ok()
        .or(lnd::downloaded_exe_path())
        .expect("lnd executable either exists locally or can be downloaded");
    (bitcoind_exe_path, lnd_exe_path)
}

pub(crate) async fn run_lnd_node(
    bitcoin_conf: bitcoind::Conf<'static>,
    lnd_conf: lnd::LndConf<'static>,
) -> (String, Lnd, BitcoinD) {
    let (bitcoind_exe, lnd_exe) = get_executables_paths();
    debug!("bitcoind: {}", &bitcoind_exe);
    debug!("lnd: {}", &lnd_exe);

    let bitcoind = BitcoinD::with_conf(bitcoind_exe, &bitcoin_conf).unwrap();

    let cookie = bitcoind.params.cookie_file.to_str().unwrap();
    let rpc_socket = bitcoind.params.rpc_socket.to_string();
    let lnd = Lnd::with_conf(
        &lnd_exe,
        &lnd_conf,
        cookie.to_string(),
        rpc_socket,
        &bitcoind,
    )
    .await
    .unwrap();

    (lnd_exe, lnd, bitcoind)
}

#[tokio::test]
async fn test_run_lnd() {
    use tonic_lnd::lnrpc::GetInfoRequest;

    let bitcoind_conf = bitcoind::Conf::default();
    let lnd_conf = LndConf::default();
    let (_lnd_exe, mut lnd, _bitcoind) = run_lnd_node(bitcoind_conf, lnd_conf).await;

    let node_info = lnd.client.lightning().get_info(GetInfoRequest {}).await;

    assert!(node_info.is_ok());
}
