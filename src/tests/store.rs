use crate::fiber::config::AnnouncedNodeName;
use crate::fiber::graph::ChannelInfo;
use crate::fiber::graph::NetworkGraphStateStore;
use crate::fiber::graph::NodeInfo;
use crate::fiber::tests::test_utils::gen_sha256_hash;
use crate::fiber::types::ChannelAnnouncement;
use crate::fiber::types::Hash256;
use crate::fiber::types::NodeAnnouncement;
use crate::fiber::types::Pubkey;
use crate::invoice::*;
use crate::store::Store;
use crate::store::CHANNEL_INFO_PREFIX;
use crate::store::NODE_INFO_PREFIX;
use crate::watchtower::*;
use ckb_jsonrpc_types::JsonBytes;
use ckb_types::packed::Bytes;
use ckb_types::packed::CellOutput;
use ckb_types::packed::OutPoint;
use ckb_types::packed::Script;
use ckb_types::prelude::*;
use musig2::CompactSignature;
use secp256k1::Keypair;
use secp256k1::PublicKey;
use secp256k1::Secp256k1;
use tempfile::tempdir;

fn gen_rand_public_key() -> PublicKey {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    PublicKey::from_keypair(&key_pair)
}

fn mock_node() -> (Pubkey, NodeInfo) {
    let node_id: Pubkey = gen_rand_public_key().into();
    let node = NodeInfo {
        node_id,
        anouncement_msg: NodeAnnouncement::new_unsigned(
            AnnouncedNodeName::from_str("node1").expect("invalid name"),
            vec![],
            node_id,
            1,
            0,
        ),
        timestamp: 0,
    };
    (node_id, node)
}

fn mock_channel() -> ChannelInfo {
    let node1: Pubkey = gen_rand_public_key().into();
    let node2: Pubkey = gen_rand_public_key().into();
    let secp = Secp256k1::new();
    let keypair = Keypair::new(&secp, &mut rand::thread_rng());
    let (xonly, _parity) = keypair.x_only_public_key();
    let rand_hash256 = gen_sha256_hash();
    ChannelInfo {
        funding_tx_block_number: 0,
        funding_tx_index: 0,
        timestamp: 0,
        node1_to_node2: None,
        node2_to_node1: None,
        announcement_msg: ChannelAnnouncement::new_unsigned(
            &node1,
            &node2,
            OutPoint::new_builder()
                .tx_hash(rand_hash256.into())
                .index(0u32.pack())
                .build(),
            Hash256::default(),
            &xonly,
            0,
            None,
        ),
    }
}

#[test]
fn test_store_invoice() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invoice_store");
    let store = Store::new(path);

    let preimage = gen_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(preimage)
        .fallback_address("address".to_string())
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .build()
        .unwrap();

    let hash = invoice.payment_hash();
    store
        .insert_invoice(invoice.clone(), Some(preimage))
        .unwrap();
    assert_eq!(store.get_invoice(hash), Some(invoice.clone()));
    assert_eq!(store.get_invoice_preimage(hash), Some(preimage));

    let invalid_hash = gen_sha256_hash();
    assert_eq!(store.get_invoice_preimage(&invalid_hash), None);
}

#[test]
fn test_store_channels() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invoice_store");
    let store = Store::new(path);

    let mut channels = vec![];
    for _ in 0..10 {
        let channel = mock_channel();
        store.insert_channel(channel.clone());
        channels.push(channel);
    }

    // sort by out_point
    channels.sort_by(|a, b| a.out_point().cmp(&b.out_point()));

    let outpoint_0 = channels[0].out_point();
    assert_eq!(
        store.get_channels(Some(outpoint_0)),
        vec![channels[0].clone()]
    );
    let (res, last_cursor) = store.get_channels_with_params(1, None, None);
    assert_eq!(res, vec![channels[0].clone()]);
    assert_eq!(res.len(), 1);

    let mut key = Vec::with_capacity(37);
    key.push(CHANNEL_INFO_PREFIX);
    key.extend_from_slice(channels[0].out_point().as_slice());
    assert_eq!(last_cursor, JsonBytes::from_bytes(key.to_vec().into()));

    let (res, _last_cursor) = store.get_channels_with_params(3, Some(last_cursor), None);
    assert_eq!(res, channels[1..=3]);
}

#[test]
fn test_store_nodes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("invoice_store");
    let store = Store::new(path);

    let mut nodes = vec![];
    for _ in 0..10 {
        let (_, node) = mock_node();
        store.insert_node(node.clone());
        nodes.push(node);
    }

    // sort by node pubkey
    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));

    let node_id = nodes[0].node_id;
    assert_eq!(store.get_nodes(Some(node_id)), vec![nodes[0].clone()]);
    let (res, last_cursor) = store.get_nodes_with_params(1, None, None);
    assert_eq!(res, vec![nodes[0].clone()]);
    assert_eq!(res.len(), 1);
    let mut key = Vec::with_capacity(34);
    key.push(NODE_INFO_PREFIX);
    key.extend_from_slice(nodes[0].node_id.serialize().as_ref());
    assert_eq!(last_cursor, JsonBytes::from_bytes(key.to_vec().into()));

    let (res, _last_cursor) = store.get_nodes_with_params(3, Some(last_cursor), None);
    assert_eq!(res, nodes[1..=3]);
}

#[test]
fn test_store_wacthtower() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("watchtower_store");
    let store = Store::new(path);

    let channel_id = gen_sha256_hash();
    let funding_tx_lock = Script::default();
    store.insert_watch_channel(channel_id, funding_tx_lock.clone());
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_tx_lock: funding_tx_lock.clone(),
            revocation_data: None
        }]
    );

    let revocation_data = RevocationData {
        commitment_number: 0,
        x_only_aggregated_pubkey: [0u8; 32],
        aggregated_signature: CompactSignature::from_bytes(&[0u8; 64]).unwrap(),
        output: CellOutput::default(),
        output_data: Bytes::default(),
    };
    store.update_revocation(channel_id, revocation_data.clone());
    assert_eq!(
        store.get_watch_channels(),
        vec![ChannelData {
            channel_id,
            funding_tx_lock,
            revocation_data: Some(revocation_data)
        }]
    );

    store.remove_watch_channel(channel_id);
    assert_eq!(store.get_watch_channels(), vec![]);
}
