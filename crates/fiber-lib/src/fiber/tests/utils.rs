use std::{borrow::Cow, str::FromStr};

use tempfile::NamedTempFile;
use tentacle::{
    multiaddr::{Multiaddr, Protocol},
    secio::PeerId,
};

use crate::utils::encrypt_decrypt_file::decrypt_from_file;
use crate::utils::encrypt_decrypt_file::encrypt_to_file;
use crate::utils::is_addr_reachable;

#[test]
fn test_is_addr_reachable_with_onion3_address() {
    let onion3_addr = Multiaddr::from_str(
        "/onion3/vww6ybal4bd7szmgncyruucpgfkqahzddi37ktceo3ah7ngmcopnpyyd:8228",
    )
    .expect("valid onion3 multiaddr");
    assert!(
        is_addr_reachable(&onion3_addr),
        "onion3 address should be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_with_private_ip() {
    let private_addr =
        Multiaddr::from_str("/ip4/192.168.1.1/tcp/8228").expect("valid private ip multiaddr");
    assert!(
        !is_addr_reachable(&private_addr),
        "private IP address should not be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_with_public_ip() {
    let public_addr =
        Multiaddr::from_str("/ip4/1.1.1.1/tcp/8228").expect("valid public ip multiaddr");
    assert!(
        is_addr_reachable(&public_addr),
        "public IP address should be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_with_public_quic_address() {
    let mut public_addr =
        Multiaddr::from_str("/ip4/1.1.1.1/udp/8228/quic-v1").expect("valid public QUIC multiaddr");
    public_addr.push(Protocol::P2P(Cow::Owned(PeerId::random().into_bytes())));

    assert!(
        is_addr_reachable(&public_addr),
        "public QUIC address should be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_with_private_quic_address() {
    let private_addr = Multiaddr::from_str("/ip4/192.168.1.1/udp/8228/quic-v1")
        .expect("valid private QUIC multiaddr");

    assert!(
        !is_addr_reachable(&private_addr),
        "private QUIC address should not be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_with_public_ipv6_quic_address() {
    let public_addr = Multiaddr::from_str("/ip6/2606:4700:4700::1111/udp/8228/quic-v1")
        .expect("valid public IPv6 QUIC multiaddr");

    assert!(
        is_addr_reachable(&public_addr),
        "public IPv6 QUIC address should be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_with_private_ipv6_quic_address() {
    let private_addr = Multiaddr::from_str("/ip6/fc00::1/udp/8228/quic-v1")
        .expect("valid private IPv6 QUIC multiaddr");

    assert!(
        !is_addr_reachable(&private_addr),
        "private IPv6 QUIC address should not be considered reachable"
    );
}

#[test]
fn test_is_addr_reachable_rejects_bare_ipv4_udp_address() {
    let bare_udp_addr =
        Multiaddr::from_str("/ip4/1.1.1.1/udp/8228").expect("valid bare IPv4 UDP multiaddr");

    assert!(
        !is_addr_reachable(&bare_udp_addr),
        "bare IPv4 UDP address should be rejected because Tentacle cannot dial it"
    );
}

#[test]
fn test_is_addr_reachable_rejects_bare_ipv6_udp_address() {
    let bare_udp_addr = Multiaddr::from_str("/ip6/2606:4700:4700::1111/udp/8228")
        .expect("valid bare IPv6 UDP multiaddr");

    assert!(
        !is_addr_reachable(&bare_udp_addr),
        "bare IPv6 UDP address should be rejected because Tentacle cannot dial it"
    );
}

#[test]
fn test_is_addr_reachable_rejects_bare_dns_udp_address() {
    let bare_udp_addr =
        Multiaddr::from_str("/dns4/example.com/udp/8228").expect("valid bare DNS UDP multiaddr");

    assert!(
        !is_addr_reachable(&bare_udp_addr),
        "bare DNS UDP address should be rejected because Tentacle cannot dial it"
    );
}

#[test]
fn test_is_addr_reachable_rejects_dns_quic_address() {
    let dns_quic_addr = Multiaddr::from_str("/dns4/example.com/udp/8228/quic-v1")
        .expect("syntactically valid DNS QUIC multiaddr");

    assert!(
        !is_addr_reachable(&dns_quic_addr),
        "DNS QUIC address should be rejected because Tentacle does not support dialing it"
    );
}

#[test]
fn test_is_addr_reachable_with_dns4_address() {
    let dns4_addr =
        Multiaddr::from_str("/dns4/example.com/tcp/8228").expect("valid dns4 multiaddr");
    assert!(
        is_addr_reachable(&dns4_addr),
        "dns4 address should be considered reachable"
    );
}

#[test]
fn test_encrypt_and_decrypt_success() {
    let password = b"not my password";
    let plain_text = b"my super secret private key data";

    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    encrypt_to_file(path, plain_text, password).unwrap();

    let decrypted = decrypt_from_file(path, password).unwrap();
    assert_eq!(plain_text.to_vec(), decrypted);
}

#[test]
fn test_decrypt_with_wrong_password_should_fail() {
    let password = b"correct_password";
    let wrong_password = b"wrong_password";
    let plain_text = b"private key data";

    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path();

    encrypt_to_file(path, plain_text, password).unwrap();

    let result = decrypt_from_file(path, wrong_password);
    assert!(
        result.is_err(),
        "Decryption should fail with wrong password"
    );
}
