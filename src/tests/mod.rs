use ckb_hash::blake2b_256;
use ckb_types::core::TransactionView;
use ckb_types::packed::CellOutput;
use ckb_types::prelude::{Builder, Entity};
use ckb_types::{packed::OutPoint, prelude::Pack};
use secp256k1::{Keypair, PublicKey, Secp256k1, SecretKey, XOnlyPublicKey};

use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
use crate::fiber::types::EcdsaSignature;
use crate::{
    fiber::{
        config::AnnouncedNodeName,
        types::{ChannelAnnouncement, NodeAnnouncement, Privkey, Pubkey},
    },
    now_timestamp_as_millis_u64,
};

pub fn gen_rand_fiber_public_key() -> Pubkey {
    gen_rand_secp256k1_public_key().into()
}

pub fn gen_rand_fiber_private_key() -> Privkey {
    gen_rand_secp256k1_private_key().into()
}

pub fn gen_rand_secp256k1_private_key() -> SecretKey {
    gen_rand_secp256k1_keypair_tuple().0
}

pub fn gen_rand_secp256k1_public_key() -> PublicKey {
    gen_rand_secp256k1_keypair_tuple().1
}

pub fn gen_rand_secp256k1_keypair() -> Keypair {
    let secp = Secp256k1::new();
    Keypair::new(&secp, &mut rand::thread_rng())
}

pub fn gen_rand_secp256k1_keypair_tuple() -> (SecretKey, PublicKey) {
    let key_pair = gen_rand_secp256k1_keypair();
    (
        SecretKey::from_keypair(&key_pair),
        PublicKey::from_keypair(&key_pair),
    )
}

pub fn gen_rand_channel_outpoint() -> OutPoint {
    let rand_slice = (0..36).map(|_| rand::random::<u8>()).collect::<Vec<u8>>();
    OutPoint::from_slice(&rand_slice).unwrap()
}

pub fn gen_rand_node_announcement() -> (Privkey, NodeAnnouncement) {
    let sk = gen_rand_fiber_private_key();
    let node_announcement = gen_node_announcement_from_privkey(&sk);
    (sk, node_announcement)
}

pub fn gen_node_announcement_from_privkey(sk: &Privkey) -> NodeAnnouncement {
    NodeAnnouncement::new(
        AnnouncedNodeName::from_str("node1").expect("valid name"),
        vec![],
        sk,
        now_timestamp_as_millis_u64(),
        0,
    )
}

pub fn create_funding_tx(x_only: &XOnlyPublicKey) -> TransactionView {
    let capacity = 100u64;
    let commitment_lock_script_args = [&blake2b_256(x_only.serialize())[0..20]].concat();

    TransactionView::new_advanced_builder()
        .cell_deps(get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock]))
        .output(
            CellOutput::new_builder()
                .capacity(capacity.pack())
                .lock(get_script_by_contract(
                    Contract::CommitmentLock,
                    commitment_lock_script_args.as_slice(),
                ))
                .build(),
        )
        .output_data(Default::default())
        .build()
}

pub fn gen_rand_channel_announcement() -> (
    Privkey,
    ChannelAnnouncement,
    TransactionView,
    Privkey,
    Privkey,
) {
    let sk1: Privkey = gen_rand_fiber_private_key();
    let sk2: Privkey = gen_rand_fiber_private_key();
    let sk = gen_rand_fiber_private_key();
    let xonly = sk.x_only_pub_key();
    let tx = create_funding_tx(&xonly);
    let outpoint = tx.output_pts_iter().next().unwrap();
    let mut channel_announcement = ChannelAnnouncement::new_unsigned(
        &sk1.pubkey(),
        &sk2.pubkey(),
        outpoint.clone(),
        &xonly,
        0,
        None,
    );
    let message = channel_announcement.message_to_sign();

    channel_announcement.ckb_signature = Some(sk.sign_schnorr(message));
    channel_announcement.node1_signature = Some(sk1.sign(message));
    channel_announcement.node2_signature = Some(sk2.sign(message));
    (sk, channel_announcement, tx, sk1, sk2)
}

pub fn create_invalid_ecdsa_signature() -> EcdsaSignature {
    let sk = Privkey::from([42u8; 32]);
    sk.sign([0u8; 32])
}
