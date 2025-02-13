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

pub enum LndBitcoinDConf<'a> {
    New(Option<bitcoind::Conf<'a>>),
    Existing(Arc<BitcoinD>),
}

impl<'a> Default for LndBitcoinDConf<'a> {
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
    pub async fn new<'a, 'b>(
        lnd_conf: Option<LndConf<'a>>,
        bitcoind_conf: LndBitcoinDConf<'b>,
    ) -> Self {
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

    pub async fn new_lnd_with_the_same_bitcoind(&self, lnd_conf: Option<LndConf<'static>>) -> Self {
        Self::new(lnd_conf, LndBitcoinDConf::Existing(self.bitcoind.clone())).await
    }

    pub async fn new_two_nodes_with_established_channel(
        bitcoind_conf: Option<bitcoind::Conf<'static>>,
        lnd_conf1: Option<LndConf<'static>>,
        lnd_conf2: Option<LndConf<'static>>,
    ) -> (Self, Self, lnd::tonic_lnd::lnrpc::ChannelPoint) {
        let mut lnd1 = Self::new(lnd_conf1, LndBitcoinDConf::New(bitcoind_conf)).await;
        lnd1.make_some_money();
        let mut lnd2 = lnd1.new_lnd_with_the_same_bitcoind(lnd_conf2).await;
        lnd2.make_some_money();

        let channel = lnd1.open_channel_with(&mut lnd2).await;

        (lnd1, lnd2, channel)
    }

    pub async fn get_info(&mut self) -> GetInfoResponse {
        self.lnd
            .client
            .lightning()
            .get_info(GetInfoRequest::default())
            .await
            .expect("get node info")
            .into_inner()
    }

    pub fn make_some_money(&self) {
        self.bitcoind
            .client
            .generate_to_address(100, &self.address)
            .expect("Blocks generated to address.");
    }

    pub async fn connect(&mut self, other: &mut Self) {
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

        let response = self.lnd.client.lightning().connect_peer(request).await;
        match response {
            Ok(_) => {}
            Err(e) if e.message().contains("already connected to peer") => {}
            Err(e) => panic!("connect peer failed: {}", e),
        }
    }

    pub async fn wait_synced_to_chain(&mut self) {
        for _ in 0..100 {
            let info = self.get_info().await;
            if info.synced_to_chain {
                return;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
        panic!("not synced to chain");
    }

    pub async fn open_channel_with(
        &mut self,
        other: &mut Self,
    ) -> lnd::tonic_lnd::lnrpc::ChannelPoint {
        self.connect(other).await;

        // We need to wait for the nodes to be synced to the chain before opening a channel.
        // This is a requirement of LND.
        self.wait_synced_to_chain().await;
        other.wait_synced_to_chain().await;

        let other_info = other.get_info().await;
        let mut request = lnd::tonic_lnd::lnrpc::OpenChannelRequest::default();
        request.node_pubkey = hex::decode(other_info.identity_pubkey).expect("valid pubkey hex");
        request.local_funding_amount = 1_000_000;
        request.sat_per_vbyte = 1;
        request.min_confs = 0;

        let channel = self
            .lnd
            .client
            .lightning()
            .open_channel_sync(request.clone())
            .await
            .expect("open channel")
            .into_inner();

        // Generate some blocks to confirm the channel.
        self.make_some_money();
        self.wait_synced_to_chain().await;
        other.wait_synced_to_chain().await;

        return channel;
    }

    pub async fn add_invoice(
        &mut self,
        value_msat: u64,
    ) -> lnd::tonic_lnd::lnrpc::AddInvoiceResponse {
        let mut request = lnd::tonic_lnd::lnrpc::Invoice::default();
        request.value_msat = value_msat as i64;
        self.lnd
            .client
            .lightning()
            .add_invoice(request)
            .await
            .expect("add hold invoice")
            .into_inner()
    }

    pub async fn get_balance_sats(&mut self) -> u64 {
        let response = self
            .lnd
            .client
            .lightning()
            .channel_balance(lnd::tonic_lnd::lnrpc::ChannelBalanceRequest::default())
            .await
            .expect("get wallet balance")
            .into_inner();
        response.local_balance.expect("local balance exists").sat
    }

    pub async fn get_balance_msats(&mut self) -> u64 {
        let response = self
            .lnd
            .client
            .lightning()
            .channel_balance(lnd::tonic_lnd::lnrpc::ChannelBalanceRequest::default())
            .await
            .expect("get wallet balance")
            .into_inner();
        response.local_balance.expect("local balance exists").msat
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
    println!("lnd 1 node_info: {:?}", lnd.get_info().await);

    let mut lnd2 = lnd.new_lnd_with_the_same_bitcoind(Default::default()).await;
    // The second node should be able to run independently of the first node.
    drop(lnd);

    let bitcoin_info = lnd2.bitcoind.client.get_network_info();
    assert!(bitcoin_info.is_ok());
    println!("lnd 2 node_info: {:?}", lnd2.get_info().await);
}

#[cfg_attr(not(feature = "lnd-tests"), ignore)]
#[tokio::test]
async fn test_run_lnd_two_nodes_with_established_channel() {
    let (_lnd1, _lnd2, channel) = LndNode::new_two_nodes_with_established_channel(
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .await;
    println!("channel: {:?}", channel);
}
