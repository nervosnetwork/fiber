use std::str::FromStr;
use std::sync::Arc;

use lnd::bitcoind::bitcoincore_rpc::bitcoin::address::NetworkChecked;
use lnd::bitcoind::bitcoincore_rpc::bitcoin::Address;
use lnd::bitcoind::bitcoincore_rpc::RpcApi;
use lnd::bitcoind::{self, BitcoinD};
use lnd::tonic_lnd::lnrpc::{GetInfoRequest, GetInfoResponse};
use lnd::{self, Lnd, LndConf};

fn get_bitcoind_exe_path() -> String {
    bitcoind::exe_path().expect("bitcoind executable does not exist. See https://docs.rs/bitcoind/0.34.3/bitcoind/fn.exe_path.html for how the bitcoind executable is searched")
}

fn get_lnd_exe_path() -> String {
    lnd::exe_path().expect("lnd executable does not exist. See https://docs.rs/lnd/0.1.6/lnd/fn.exe_path.html for how the lnd executable is searched")
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
    pub address: Address<NetworkChecked>,
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
        let mut lnd = Lnd::with_conf(
            &lnd_exe,
            &lnd_conf,
            cookie.to_string(),
            rpc_socket,
            bitcoind.as_ref(),
        )
        .await
        .unwrap();

        use lnd::tonic_lnd::lnrpc::{AddressType, NewAddressRequest};
        let mut request = NewAddressRequest::default();
        request.set_type(AddressType::TaprootPubkey);
        let client = lnd.client.lightning();
        let address = client
            .new_address(request)
            .await
            .unwrap()
            .into_inner()
            .address;

        let address = Address::from_str(&address)
            .expect("valid address")
            .assume_checked();

        Self {
            lnd,
            bitcoind,
            address,
        }
    }

    async fn new_lnd_with_the_same_bitcoind(&self, lnd_conf: Option<LndConf<'static>>) -> Self {
        Self::new(lnd_conf, LndBitcoinDConf::Existing(self.bitcoind.clone())).await
    }

    async fn get_info(&mut self) -> GetInfoResponse {
        self.lnd
            .client
            .lightning()
            .get_info(GetInfoRequest::default())
            .await
            .expect("get node info")
            .into_inner()
    }

    fn make_some_money(&self) {
        self.bitcoind
            .client
            .generate_to_address(100, &self.address)
            .expect("Blocks generated to address.");
    }

    async fn connect(&mut self, other: &mut Self) {
        let other_info = other.get_info().await;
        let mut request = lnd::tonic_lnd::lnrpc::ConnectPeerRequest::default();
        let mut address = lnd::tonic_lnd::lnrpc::LightningAddress::default();
        address.pubkey = other_info.identity_pubkey;
        address.host = other
            .lnd
            .listen_url
            .clone()
            .expect("have listening address");
        request.addr = Some(address);
        request.perm = false;

        let response = self
            .lnd
            .client
            .lightning()
            .connect_peer(request)
            .await
            .expect("open channel")
            .into_inner();
        println!("connect response: {:?}", response);
    }

    async fn open_channel_with(&mut self, other: &mut Self) {
        self.connect(other).await;

        let other_info = other.get_info().await;
        let mut request = lnd::tonic_lnd::lnrpc::OpenChannelRequest::default();
        request.node_pubkey_string = other_info.identity_pubkey;
        request.local_funding_amount = 1_000_000;
        request.sat_per_vbyte = 1;
        request.min_confs = 0;

        for _ in 0..10 {
            match self
                .lnd
                .client
                .lightning()
                .open_channel_sync(request.clone())
                .await
            {
                Ok(response) => {
                    println!("open_channel response: {:?}", response);
                    return;
                }
                Err(e) => {
                    // self.make_some_money();
                    self.bitcoind
                        .client
                        .generate_to_address(100, &self.address)
                        .expect("Blocks generated to address.");
                    println!("open_channel error: {:?}", e);
                }
            }
        }
        panic!("open_channel failed");
    }
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_one_node() {
    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    let bitcoin_info = lnd.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    println!("node_info: {:?}", lnd.get_info().await);
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_two_nodes_with_the_same_bitcoind() {
    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    let bitcoin_info = lnd.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    println!("node_info: {:?}", lnd.get_info().await);

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

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_two_nodes_with_established_channel() {
    let mut lnd = LndNode::new(Default::default(), Default::default()).await;
    let bitcoin_info = lnd.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    println!("node_info: {:?}", lnd.get_info().await);
    lnd.make_some_money();

    let mut lnd2 = lnd.new_lnd_with_the_same_bitcoind(Default::default()).await;
    let bitcoin_info = lnd2.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    println!("node_info: {:?}", lnd.get_info().await);
    lnd.make_some_money();
    lnd2.make_some_money();

    lnd.open_channel_with(&mut lnd2).await;
}
