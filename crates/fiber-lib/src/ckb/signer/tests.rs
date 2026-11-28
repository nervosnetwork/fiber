use super::*;

#[test]
fn test_local_signer_creation() {
    // Use a fixed test secret key
    let secret_key_bytes = [1u8; 32];
    let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();

    let signer = LocalSigner::new(secret_key);

    // Verify pubkey is derived correctly
    let expected_pubkey = secret_key.public_key(SECP256K1);
    assert_eq!(signer.pubkey(), &expected_pubkey);

    // Verify pubkey_hash is 20 bytes
    assert_eq!(signer.pubkey_hash().len(), 20);
}

#[test]
fn test_sign_recoverable() {
    let secret_key_bytes = [42u8; 32];
    let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();
    let signer = LocalSigner::new(secret_key);

    let message = [0xab; 32];
    let signature = signer.sign_recoverable(&message);

    // Signature should be 65 bytes
    assert_eq!(signature.len(), 65);

    // Verify the signature is deterministic
    let signature2 = signer.sign_recoverable(&message);
    assert_eq!(signature, signature2);
}

#[test]
fn test_signature_matches_direct_signing() {
    // Verify that LocalSigner produces the same signature as direct secp256k1 signing
    let secret_key_bytes = [99u8; 32];
    let secret_key = SecretKey::from_slice(&secret_key_bytes).unwrap();
    let signer = LocalSigner::new(secret_key);

    let message = [0x12; 32];

    // Sign using LocalSigner
    let signer_sig = signer.sign_recoverable(&message);

    // Sign directly using secp256k1
    let msg = Message::from_digest(message);
    let direct_sig = SECP256K1.sign_ecdsa_recoverable(&msg, &secret_key);
    let (recov_id, data) = direct_sig.serialize_compact();
    let mut direct_sig_bytes = [0u8; 65];
    direct_sig_bytes[0..64].copy_from_slice(&data[0..64]);
    direct_sig_bytes[64] = i32::from(recov_id) as u8;

    // They should be identical
    assert_eq!(signer_sig, direct_sig_bytes);
}

#[tokio::test]
async fn test_sign_funding_tx_rejects_peer_input_using_local_lock() {
    use crate::ckb::contracts::{get_cell_deps_by_contracts, get_script_by_contract, Contract};
    use ckb_sdk::traits::OffchainTransactionDependencyProvider;
    use ckb_types::core::{Capacity, TransactionView};
    use ckb_types::packed::{Byte32, CellInput, CellOutput, OutPoint};
    use ckb_types::prelude::{Builder, Entity, Pack, Unpack};

    let secret_key = SecretKey::from_slice(&[7u8; 32]).unwrap();
    let signer = LocalSigner::new(secret_key);
    let local_lock = get_script_by_contract(Contract::Secp256k1Lock, signer.pubkey_hash());
    let peer_lock = get_script_by_contract(Contract::Secp256k1Lock, &[0x99u8; 20][..]);
    let cell_deps = get_cell_deps_by_contracts(vec![Contract::Secp256k1Lock])
        .await
        .expect("secp256k1 cell deps");

    let local_outpoint = OutPoint::new(Byte32::from_slice(&[1u8; 32]).unwrap(), 0);
    let peer_outpoint = OutPoint::new(Byte32::from_slice(&[2u8; 32]).unwrap(), 0);

    let local_cell = CellOutput::new_builder()
        .capacity(Capacity::shannons(100_000_000_000).pack())
        .lock(local_lock.clone())
        .build();
    let peer_cell = CellOutput::new_builder()
        .capacity(Capacity::shannons(100_000_000_000).pack())
        .lock(peer_lock)
        .build();

    // Case 1: local input + peer input with peer's own lock -> signing succeeds.
    let mut provider = OffchainTransactionDependencyProvider::default();
    provider.cells.insert(
        (local_outpoint.tx_hash().unpack(), 0),
        (local_cell.clone(), ckb_types::bytes::Bytes::new()),
    );
    provider.cells.insert(
        (peer_outpoint.tx_hash().unpack(), 0),
        (peer_cell, ckb_types::bytes::Bytes::new()),
    );
    let tx = TransactionView::new_advanced_builder()
        .cell_deps(cell_deps.clone())
        .input(CellInput::new(local_outpoint.clone(), 0))
        .input(CellInput::new(peer_outpoint.clone(), 0))
        .output(local_cell.clone())
        .output_data(ckb_types::packed::Bytes::default())
        .witnesses(vec![
            ckb_types::packed::Bytes::default(),
            ckb_types::packed::Bytes::default(),
        ])
        .build();
    let allowed = HashSet::from([local_outpoint.clone()]);
    let signed = signer
        .sign_funding_tx_with_provider(tx.into(), &provider, &local_lock, Some(&allowed))
        .await
        .expect("local input + distinct peer lock should sign");
    assert!(!signed.into_inner().unwrap().witnesses().is_empty());

    // Case 2: peer injects an input with local_lock alongside local input (same script group).
    provider.cells.insert(
        (peer_outpoint.tx_hash().unpack(), 0),
        (local_cell.clone(), ckb_types::bytes::Bytes::new()),
    );
    let malicious_tx = TransactionView::new_advanced_builder()
        .cell_deps(cell_deps.clone())
        .input(CellInput::new(local_outpoint.clone(), 0))
        .input(CellInput::new(peer_outpoint.clone(), 0))
        .output(local_cell.clone())
        .output_data(ckb_types::packed::Bytes::default())
        .witnesses(vec![
            ckb_types::packed::Bytes::default(),
            ckb_types::packed::Bytes::default(),
        ])
        .build();
    let err = signer
        .sign_funding_tx_with_provider(
            malicious_tx.clone().into(),
            &provider,
            &local_lock,
            Some(&allowed),
        )
        .await
        .expect_err("peer input using local lock must be rejected before signing");
    assert!(
        matches!(
            err,
            FundingError::PeerInputUsesOurFundingLock { input_index: 1 }
        ),
        "expected PeerInputUsesOurFundingLock at index 1, got {err:?}"
    );

    // Case 3: node contributed 0 inputs (allowed set is empty), peer injected local_lock input.
    let zero_local_tx = TransactionView::new_advanced_builder()
        .cell_deps(cell_deps)
        .input(CellInput::new(peer_outpoint, 0))
        .output(local_cell)
        .output_data(ckb_types::packed::Bytes::default())
        .witnesses(vec![ckb_types::packed::Bytes::default()])
        .build();
    let empty_allowed = HashSet::new();
    let err = signer
        .sign_funding_tx_with_provider(
            zero_local_tx.into(),
            &provider,
            &local_lock,
            Some(&empty_allowed),
        )
        .await
        .expect_err("peer input using local lock must be rejected when local funded 0 inputs");
    assert!(
        matches!(
            err,
            FundingError::PeerInputUsesOurFundingLock { input_index: 0 }
        ),
        "expected PeerInputUsesOurFundingLock at index 0, got {err:?}"
    );
}
