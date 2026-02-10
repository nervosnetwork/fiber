#![no_main]

use libfuzzer_sys::fuzz_target;
use molecule::prelude::*;

use fnn::fiber::gen::fiber as molecule_fiber;
use fnn::fiber::gen::gossip as molecule_gossip;
use fnn::fiber::types::{
    ChannelAnnouncement, ChannelUpdate, NodeAnnouncement, TlcErr,
};

fuzz_target!(|data: &[u8]| {
    // Fuzz molecule -> Rust TryFrom conversions for gossip and channel types.
    // These parse untrusted data from P2P peers and must never panic.

    // TlcErr: received in RemoveTlc error packets from peers
    if let Ok(mol) = molecule_fiber::TlcErr::from_slice(data) {
        let _ = TlcErr::try_from(mol);
    }

    // NodeAnnouncement: gossip messages from any peer
    if let Ok(mol) = molecule_gossip::NodeAnnouncement::from_slice(data) {
        let _ = NodeAnnouncement::try_from(mol);
    }

    // ChannelAnnouncement: gossip messages from any peer
    if let Ok(mol) = molecule_gossip::ChannelAnnouncement::from_slice(data) {
        let _ = ChannelAnnouncement::try_from(mol);
    }

    // ChannelUpdate: channel update messages
    if let Ok(mol) = molecule_fiber::ChannelUpdate::from_slice(data) {
        let _ = ChannelUpdate::try_from(mol);
    }
});
