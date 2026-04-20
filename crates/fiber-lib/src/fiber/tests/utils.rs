use std::str::FromStr;

use tempfile::NamedTempFile;
use tentacle::multiaddr::Multiaddr;

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
