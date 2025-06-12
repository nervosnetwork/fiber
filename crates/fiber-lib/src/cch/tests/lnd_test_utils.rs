use std::time::Duration;
use std::{str::FromStr as _, sync::Arc};

use futures::StreamExt;
use lightning_invoice::Bolt11Invoice;
use lnd::{
    self,
    bitcoind::{
        self,
        bitcoincore_rpc::{
            bitcoin::{address::NetworkChecked, Address},
            RpcApi,
        },
        BitcoinD,
    },
    tonic_lnd::lnrpc::{GetInfoRequest, GetInfoResponse},
    Lnd, LndConf,
};
use tokio::select;

use crate::cch::actor::LndConnectionInfo;

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

impl Default for LndBitcoinDConf<'_> {
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
    pub async fn new(lnd_conf: Option<LndConf<'_>>, bitcoind_conf: LndBitcoinDConf<'_>) -> Self {
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
        for _ in 1..=10 {
            match client.new_address(request.clone()).await {
                Ok(response) => {
                    return Self {
                        lnd,
                        bitcoind,
                        address: Address::from_str(&response.into_inner().address)
                            .expect("valid address")
                            .assume_checked(),
                    };
                }
                Err(e)
                    if e.message()
                        .contains("the RPC server is in the process of starting up, but not yet ready to accept calls") =>
                {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
                Err(e) => panic!("new address failed: {}", e),
            }
        }
        panic!("Creating new address failed, server is still starting");
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

    pub fn get_lnd_connection_info(&self) -> LndConnectionInfo {
        LndConnectionInfo {
            uri: self.lnd.grpc_url.clone().try_into().expect("valid uri"),
            cert: hex::decode(&self.lnd.tls_cert).ok(),
            macaroon: hex::decode(&self.lnd.admin_macaroon).ok(),
        }
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
        let address = lnd::tonic_lnd::lnrpc::LightningAddress {
            pubkey: other_info.identity_pubkey,
            host: other
                .lnd
                .listen_url
                .clone()
                .expect("have listening address"),
        };
        let request = lnd::tonic_lnd::lnrpc::ConnectPeerRequest {
            addr: Some(address),
            perm: false,
            ..Default::default()
        };
        for _ in 1..=10 {
            let response = self
                .lnd
                .client
                .lightning()
                .connect_peer(request.clone())
                .await;
            match response {
                Ok(_) => return,
                Err(e) if e.message().contains("already connected to peer") => return,
                Err(e)
                    if e.message()
                        .contains("server is still in the process of starting") =>
                {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                }
                Err(e) => panic!("connect peer failed: {}", e),
            }
        }
        panic!("Connect peer failed after 5 retries, server is still starting");
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

    pub async fn get_active_channels(&mut self) -> Vec<lnd::tonic_lnd::lnrpc::Channel> {
        self.lnd
            .client
            .lightning()
            .list_channels(lnd::tonic_lnd::lnrpc::ListChannelsRequest {
                active_only: true,
                ..Default::default()
            })
            .await
            .expect("get channel info")
            .into_inner()
            .channels
    }

    pub async fn wait_for_channel_to_be_active(
        &mut self,
        channel_outpoint: &lnd::tonic_lnd::lnrpc::ChannelPoint,
    ) {
        let channel_outpoint_str = channel_outpoint_to_string(channel_outpoint);
        for _ in 1..=10 {
            let channels = self.get_active_channels().await;
            for channel in channels {
                if channel.channel_point == channel_outpoint_str {
                    return;
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        panic!(
            "Opened channel but failed to wait for confirmation: {:?}",
            channel_outpoint
        );
    }

    /// Open channel with another node. Returns the channel point.
    /// self will have some money in the channel, while other will have none.
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
        let request = lnd::tonic_lnd::lnrpc::OpenChannelRequest {
            node_pubkey: hex::decode(other_info.identity_pubkey).expect("valid pubkey hex"),
            local_funding_amount: 1_000_000,
            sat_per_vbyte: 1,
            min_confs: 0,
            ..Default::default()
        };

        let channel = self
            .lnd
            .client
            .lightning()
            .open_channel_sync(request.clone())
            .await
            .expect("open channel")
            .into_inner();

        // Confirm that the channel is now active.
        self.make_some_money();
        self.wait_synced_to_chain().await;
        other.wait_synced_to_chain().await;
        self.wait_for_channel_to_be_active(&channel).await;
        other.wait_for_channel_to_be_active(&channel).await;
        return channel;
    }

    pub async fn add_invoice(
        &mut self,
        value_msat: u64,
    ) -> lnd::tonic_lnd::lnrpc::AddInvoiceResponse {
        let request = lnd::tonic_lnd::lnrpc::Invoice {
            value_msat: value_msat as i64,
            ..Default::default()
        };
        self.lnd
            .client
            .lightning()
            .add_invoice(request)
            .await
            .expect("add hold invoice")
            .into_inner()
    }

    pub async fn send_payment(&self, invoice: &Bolt11Invoice) {
        let lnd_connection = self.get_lnd_connection_info();
        let mut lnd_client = lnd_connection
            .create_router_client()
            .await
            .expect("create lnd client");
        let timeout_seconds = 10;
        let request = lnd_grpc_tonic_client::routerrpc::SendPaymentRequest {
            payment_request: invoice.to_string(),
            timeout_seconds,
            ..Default::default()
        };
        let mut stream = lnd_client
            .send_payment_v2(request)
            .await
            .expect("call send_payment_v2")
            .into_inner();
        let mut ticker = tokio::time::interval(Duration::from_secs(timeout_seconds as u64));
        loop {
            select! {
                Some(payment) = stream.next() => {
                    let payment = payment.expect("payment");
                    use lnd_grpc_tonic_client::lnrpc::payment::PaymentStatus;
                    if PaymentStatus::InFlight == payment.status.try_into().expect("payment status") {
                        return;
                    }
                }
                _ = ticker.tick() => {
                    panic!("payment failed");
                }
            }
        }
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

fn channel_outpoint_to_string(channel_outpoint: &lnd::tonic_lnd::lnrpc::ChannelPoint) -> String {
    let output_index = channel_outpoint.output_index;
    let funding_txid = match channel_outpoint
        .funding_txid
        .clone()
        .expect("funding_txid exists")
    {
        lnd::tonic_lnd::lnrpc::channel_point::FundingTxid::FundingTxidBytes(mut bytes) => {
            // Don't know why the bytes returned from open_channel_sync are reversed with
            // the order of the channel_point string returned from list_channels.
            bytes.reverse();
            hex::encode(bytes)
        }
        lnd::tonic_lnd::lnrpc::channel_point::FundingTxid::FundingTxidStr(str) => str,
    };
    format!("{}:{}", funding_txid, output_index)
}
