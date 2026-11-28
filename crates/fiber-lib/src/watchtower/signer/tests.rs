use ckb_types::core::TransactionBuilder;
use fiber_types::OnchainKeyPurpose;

use super::*;

#[test]
fn local_request_signs_and_verifies() {
    let privkey = Privkey::from(&[99u8; 32]);
    let pubkey = privkey.pubkey();
    let unsigned_tx = TransactionBuilder::default().build().data();
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: unsigned_tx,
    };
    let signer = WatchtowerSigner::local(privkey);
    let outcome = signer.request_onchain(content.clone()).expect("sign ok");
    let WatchtowerSignOutcome::Ready(signature) = outcome else {
        panic!("expected Ready");
    };
    verify_onchain_signature(&pubkey, &content, &signature).expect("verify ok");
    let wrong = Privkey::from(&[100u8; 32]).pubkey();
    assert!(verify_onchain_signature(&wrong, &content, &signature).is_err());
}

#[test]
fn external_request_rejects_local_key_fallback() {
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: TransactionBuilder::default().build().data(),
    };
    assert!(WatchtowerSigner::external()
        .request_onchain_with_key(content, Some(Privkey::from(&[99; 32])))
        .is_err());
}

#[test]
fn external_request_awaits_submission() {
    let unsigned_tx = TransactionBuilder::default().build().data();
    let content = OnchainSigningContent {
        key_purpose: OnchainKeyPurpose::Settlement,
        transaction: unsigned_tx,
    };
    let outcome = WatchtowerSigner::external()
        .request_onchain(content)
        .expect("outcome ok");
    assert!(matches!(
        outcome,
        WatchtowerSignOutcome::AwaitingExternal { .. }
    ));
}
