use super::*;

fn node_announcement_with_raw_name(raw_name: [u8; 32]) -> molecule_gossip::NodeAnnouncement {
    let private_key = Privkey::from_slice(&[42u8; 32]);
    let announcement = NodeAnnouncement::new_signed(
        AnnouncedNodeName::from_string("valid-node").expect("valid node name"),
        FeatureVector::default(),
        vec![],
        &private_key,
        Hash256::default(),
        1,
        0,
        UdtCfgInfos::default(),
        "test".to_string(),
    );
    let molecule_announcement = molecule_gossip::NodeAnnouncement::from(announcement);

    molecule_gossip::NodeAnnouncement::new_builder()
        .signature(molecule_announcement.signature())
        .features(molecule_announcement.features())
        .timestamp(molecule_announcement.timestamp())
        .node_id(molecule_announcement.node_id())
        .version(molecule_announcement.version())
        .node_name(u8_32_as_byte_32(&raw_name))
        .address(molecule_announcement.address())
        .chain_hash(molecule_announcement.chain_hash())
        .auto_accept_min_ckb_funding_amount(
            molecule_announcement.auto_accept_min_ckb_funding_amount(),
        )
        .udt_cfg_infos(molecule_announcement.udt_cfg_infos())
        .build()
}

fn raw_node_name(name: &[u8]) -> [u8; 32] {
    let mut raw_name = [0u8; 32];
    raw_name[..name.len()].copy_from_slice(name);
    raw_name
}

#[test]
fn node_announcement_from_molecule_rejects_non_utf8_node_name() {
    let mut raw_name = [0u8; 32];
    raw_name[0] = b'f';
    raw_name[1] = 0xff;

    let err = NodeAnnouncement::try_from(node_announcement_with_raw_name(raw_name))
        .expect_err("invalid UTF-8 node_name must be rejected");
    assert!(err.to_string().contains("Invalid node_name"));
}

#[test]
fn node_announcement_from_molecule_rejects_control_chars_in_node_name() {
    for raw_name in [
        raw_node_name(b"\x1bc"),
        raw_node_name(b"bad\x07name"),
        raw_node_name("bad\u{85}name".as_bytes()),
    ] {
        let err = NodeAnnouncement::try_from(node_announcement_with_raw_name(raw_name))
            .expect_err("control chars in node_name must be rejected");
        assert!(err.to_string().contains("Invalid node_name"));
    }
}

#[test]
fn announced_node_name_allows_printable_utf8() {
    let name = "节点-café";
    let announced_name =
        AnnouncedNodeName::from_string(name).expect("printable UTF-8 node name is valid");
    assert_eq!(announced_name.as_str(), name);

    let announcement = NodeAnnouncement::try_from(node_announcement_with_raw_name(raw_node_name(
        name.as_bytes(),
    )))
    .expect("printable UTF-8 node_name from molecule is valid");
    assert_eq!(announcement.node_name.as_str(), name);
}

#[test]
fn malformed_announced_node_name_debug_and_serialize_do_not_panic() {
    let mut raw_name = [0u8; 32];
    raw_name[0] = b'f';
    raw_name[1] = 0xff;
    let node_name = AnnouncedNodeName(raw_name);

    let debug_result = std::panic::catch_unwind(|| format!("{node_name:?}"));
    assert!(debug_result.is_ok());

    let serialize_result = std::panic::catch_unwind(|| serde_json::to_string(&node_name));
    assert!(serialize_result.is_ok());
    assert!(serialize_result.expect("panic checked").is_err());
}
