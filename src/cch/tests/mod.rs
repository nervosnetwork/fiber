use std::sync::Arc;

use lnd::bitcoind::bitcoincore_rpc::RpcApi;
use lnd::bitcoind::{self, BitcoinD};
use lnd::{self, Lnd, LndConf};

fn get_bitcoind_exe_path() -> String {
    bitcoind::exe_path()
        .or(bitcoind::downloaded_exe_path())
        .expect("bitcoind executable either exists locally or can be downloaded")
}

fn get_lnd_exe_path() -> String {
    lnd::exe_path()
        .ok()
        .or(lnd::downloaded_exe_path())
        .expect("lnd executable either exists locally or can be downloaded")
}

pub(crate) enum LndBitcoinDConf {
    New(Option<bitcoind::Conf<'static>>),
    Existing(Arc<BitcoinD>),
}

impl Default for LndBitcoinDConf {
    fn default() -> Self {
        Self::New(None)
    }
}

pub struct LndNode {
    pub lnd: Lnd,
    pub bitcoind: Arc<BitcoinD>,
}

impl LndNode {
    async fn new<'a>(lnd_conf: Option<LndConf<'static>>, bitcoind_conf: LndBitcoinDConf) -> Self {
        let bitcoind = match bitcoind_conf {
            LndBitcoinDConf::New(conf) => {
                let conf = conf.unwrap_or_default();
                let bitcoind =
                    BitcoinD::with_conf(get_bitcoind_exe_path(), &conf).expect("run bitcoind");
                Arc::new(bitcoind)
            }
            LndBitcoinDConf::Existing(bitcoind) => bitcoind,
        };
        let cookie = bitcoind
            .params
            .cookie_file
            .to_str()
            .expect("get bitcoind cookie");
        let rpc_socket = bitcoind.params.rpc_socket.to_string();

        let lnd_exe = get_lnd_exe_path();
        let lnd_conf = lnd_conf.unwrap_or_default();
        let lnd = Lnd::with_conf(
            &lnd_exe,
            &lnd_conf,
            cookie.to_string(),
            rpc_socket,
            bitcoind.as_ref(),
        )
        .await
        .unwrap();

        Self { lnd, bitcoind }
    }

    async fn new_lnd_with_the_same_bitcoind(&self, lnd_conf: Option<LndConf<'static>>) -> Self {
        Self::new(lnd_conf, LndBitcoinDConf::Existing(self.bitcoind.clone())).await
    }
}

#[tokio::test]
async fn test_run_lnd_one_node() {
    use tonic_lnd::lnrpc::GetInfoRequest;

    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    let bitcoin_info = lnd.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    let node_info = lnd.lnd.client.lightning().get_info(GetInfoRequest {}).await;
    assert!(node_info.is_ok());
}

#[tokio::test]
async fn test_run_lnd_two_nodes_with_the_same_bitcoind() {
    use tonic_lnd::lnrpc::GetInfoRequest;

    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    let bitcoin_info = lnd.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    let node_info = lnd.lnd.client.lightning().get_info(GetInfoRequest {}).await;
    assert!(node_info.is_ok());

    let mut lnd2 = lnd.new_lnd_with_the_same_bitcoind(Default::default()).await;
    // The second node should be able to run independently of the first node.
    drop(lnd);

    let bitcoin_info = lnd2.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    let node_info = lnd2
        .lnd
        .client
        .lightning()
        .get_info(GetInfoRequest {})
        .await;
    assert!(node_info.is_ok());
}
