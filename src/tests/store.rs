use crate::fiber::config::AnnouncedNodeName;
use crate::fiber::gossip::GossipMessageStore;
use crate::fiber::tests::test_utils::gen_sha256_hash;
use crate::fiber::types::BroadcastMessageID;
use crate::fiber::types::BroadcastMessageWithTimestamp;
use crate::fiber::types::ChannelAnnouncement;
use crate::fiber::types::ChannelUpdate;
use crate::fiber::types::Cursor;
use crate::fiber::types::Hash256;
use crate::fiber::types::NodeAnnouncement;
use crate::fiber::types::Privkey;
use crate::invoice::*;
use crate::now_timestamp;
use crate::store::Store;
use crate::watchtower::*;
use ckb_types::packed::Bytes;
use ckb_types::packed::CellOutput;
use ckb_types::packed::OutPoint;
use ckb_types::packed::Script;
use ckb_types::prelude::*;
use musig2::CompactSignature;
use secp256k1::Keypair;
use secp256k1::Secp256k1;
use secp256k1::SecretKey;
use tempfile::tempdir;

fn gen_rand_key_pair() -> Keypair {
    let secp = Secp256k1::new();
    Keypair::new(&secp, &mut rand::thread_rng())
}

fn gen_rand_private_key() -> SecretKey {
    gen_rand_key_pair().secret_key()
}

fn mock_node() -> (Privkey, NodeAnnouncement) {
    let sk: Privkey = gen_rand_private_key().into();
    (
        sk.clone(),
        NodeAnnouncement::new(
            AnnouncedNodeName::from_str("node1").expect("invalid name"),
            vec![],
            &sk,
            now_timestamp(),
            0,
        ),
    )
}

fn mock_channel() -> ChannelAnnouncement {
    let sk1: Privkey = gen_rand_private_key().into();
    let sk2: Privkey = gen_rand_private_key().into();
    let keypair = gen_rand_key_pair();
    let (xonly, _parity) = keypair.x_only_public_key();
    let rand_hash256 = gen_sha256_hash();
    ChannelAnnouncement::new_unsigned(
        &sk1.pubkey(),
        &sk2.pubkey(),
        OutPoint::new_builder()
            .tx_hash(rand_hash256.into())
            .index(0u32.pack())
            .build(),
        Hash256::default(),
        &xonly,
        0,
        None,
    )
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

    assert_eq!(store.get_invoice_status(hash), Some(CkbInvoiceStatus::Open));
    assert_eq!(store.get_invoice_status(&gen_sha256_hash()), None);

    let status = CkbInvoiceStatus::Paid;
    store.update_invoice_status(hash, status).unwrap();
    assert_eq!(store.get_invoice_status(hash), Some(status));
}

#[test]
fn test_store_get_broadcast_messages_iter() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gossip_store");
    let store = Store::new(path);

    let timestamp = now_timestamp();
    let channel_announcement = mock_channel();
    let outpoint = channel_announcement.out_point().clone();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let default_cursor = Cursor::default();
    let mut iter = store
        .get_broadcast_messages_iter(&default_cursor)
        .into_iter();
    assert_eq!(
        iter.next(),
        Some(BroadcastMessageWithTimestamp::ChannelAnnouncement(
            timestamp,
            channel_announcement
        )),
    );
    assert_eq!(iter.next(), None);
    let cursor = Cursor::new(timestamp, BroadcastMessageID::ChannelAnnouncement(outpoint));
    let mut iter = store.get_broadcast_messages_iter(&cursor).into_iter();
    assert_eq!(iter.next(), None);
}

#[test]
fn test_store_get_broadcast_messages() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gossip_store");
    let store = Store::new(path);

    let timestamp = now_timestamp();
    let channel_announcement = mock_channel();
    let outpoint = channel_announcement.out_point().clone();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let default_cursor = Cursor::default();
    let result = store.get_broadcast_messages(&default_cursor, None);
    assert_eq!(
        result,
        vec![BroadcastMessageWithTimestamp::ChannelAnnouncement(
            timestamp,
            channel_announcement
        )],
    );
    let cursor = Cursor::new(timestamp, BroadcastMessageID::ChannelAnnouncement(outpoint));
    let result = store.get_broadcast_messages(&cursor, None);
    assert_eq!(result, vec![]);
}

#[test]
fn test_store_save_channel_announcement() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gossip_store");
    let store = Store::new(path);

    let timestamp = now_timestamp();
    let channel_announcement = mock_channel();
    store.save_channel_announcement(timestamp, channel_announcement.clone());
    let new_channel_announcement =
        store.get_latest_channel_announcement(channel_announcement.out_point());
    assert_eq!(
        new_channel_announcement,
        Some((timestamp, channel_announcement))
    );
}

#[test]
fn test_store_save_channel_update() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gossip_store");
    let store = Store::new(path);

    let flags_for_update_of_node1 = 0;
    let channel_update_of_node1 = ChannelUpdate::new_unsigned(
        Hash256::default(),
        OutPoint::new_builder()
            .tx_hash(gen_sha256_hash().into())
            .index(0u32.pack())
            .build(),
        1,
        flags_for_update_of_node1,
        0,
        0,
        0,
        0,
        0,
    );
    let out_point = channel_update_of_node1.channel_outpoint.clone();
    store.save_channel_update(channel_update_of_node1.clone());
    assert_eq!(
        store.get_latest_channel_update(&out_point, true).as_ref(),
        Some(&channel_update_of_node1)
    );
    assert_eq!(store.get_latest_channel_update(&out_point, false), None);

    let mut channel_update_of_node2 = channel_update_of_node1.clone();
    let flags_for_update_of_node2 = 1;
    channel_update_of_node2.message_flags = flags_for_update_of_node2;
    // Note that per discussion in Notion, we don't handle the rare case of two channel updates having the same timestamp.
    // In the current implementation, channel update from one side with the same timestamp will not overwrite the existing one
    // from the other side. So we have to set the timestamp to be different.
    channel_update_of_node2.timestamp = 2;
    store.save_channel_update(channel_update_of_node2.clone());
    assert_eq!(
        store.get_latest_channel_update(&out_point, false).as_ref(),
        Some(&channel_update_of_node2)
    );
    assert_eq!(
        store.get_latest_channel_update(&out_point, true).as_ref(),
        Some(&channel_update_of_node1)
    );
}

#[test]
fn test_store_save_node_announcement() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gossip_store");
    let store = Store::new(path);

    let (sk, node_announcement) = mock_node();
    let pk = sk.pubkey();
    store.save_node_announcement(node_announcement.clone());
    let new_node_announcement = store.get_latest_node_announcement(&pk);
    assert_eq!(new_node_announcement, Some(node_announcement));
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
