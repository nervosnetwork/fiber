use crate::fiber::channel::{occupied_capacity, reserved_capacity};
use crate::fiber::fee::checked_calculate_commitment_tx_fee;
#[cfg(feature = "watchtower")]
use crate::watchtower::build_settlement_transaction;
#[cfg(feature = "watchtower")]
use ckb_types::core::TransactionBuilder;
#[cfg(feature = "watchtower")]
use ckb_types::packed::{Bytes, CellInput, CellOutput, Script, WitnessArgs};
#[cfg(feature = "watchtower")]
use ckb_types::prelude::Builder;
use fiber_types::CommitmentContractFeatures;
#[cfg(feature = "watchtower")]
use fiber_types::{Hash256, HashAlgorithm, Privkey, SettlementData, TLCId};
#[cfg(feature = "watchtower")]
use molecule::prelude::Entity;

#[cfg(feature = "watchtower")]
use crate::fiber::channel::{settlement_data_to_witness, XUDT_COMPATIBLE_WITNESS};

#[test]
#[cfg(feature = "watchtower")]
fn settlement_transaction_v1_witness_is_12_bytes_larger_per_pending_htlc() {
    let settlement_data = SettlementData {
        local_amount: 2_000,
        remote_amount: 3_000,
        tlcs: vec![fiber_types::SettlementTlc {
            tlc_id: TLCId::Offered(0),
            hash_algorithm: HashAlgorithm::CkbHash,
            payment_amount: 1_000,
            payment_hash: Hash256::from([8u8; 32]),
            expiry: 0,
            local_key: Privkey::from(&[1u8; 32]),
            remote_key: Privkey::from(&[2u8; 32]).pubkey(),
        }],
    };
    let unlock = vec![0u8; 67];
    let settlement_transaction = |version| {
        let witness = [
            XUDT_COMPATIBLE_WITNESS.as_slice(),
            &[1u8],
            settlement_data_to_witness(
                &settlement_data,
                false,
                version,
                Privkey::from(&[3u8; 32]),
                Privkey::from(&[4u8; 32]).pubkey(),
            )
            .as_slice(),
            unlock.as_slice(),
        ]
        .concat();
        build_settlement_transaction(
            TransactionBuilder::default()
                .input(CellInput::default())
                .output(CellOutput::new_builder().lock(Script::default()).build())
                .output_data(Bytes::default()),
            witness,
            WitnessArgs::default(),
        )
        .build()
    };

    let legacy_tx = settlement_transaction(CommitmentContractFeatures::LEGACY);
    let v1_tx = settlement_transaction(CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH);
    let legacy_witness_len = legacy_tx.witnesses().get(0).unwrap().raw_data().len();
    let v1_witness_len = v1_tx.witnesses().get(0).unwrap().raw_data().len();

    assert_eq!(v1_witness_len - legacy_witness_len, 12);
    assert_eq!(
        v1_tx.data().serialized_size_in_block() - legacy_tx.data().serialized_size_in_block(),
        12
    );
}

#[test]
fn checked_commitment_tx_fee_allows_intermediate_u64_overflow_when_final_fee_fits() {
    let tx_size =
        checked_calculate_commitment_tx_fee(1_000, &None, CommitmentContractFeatures::LEGACY)
            .expect("fee rate 1000 should return commitment tx size");
    assert!(tx_size > 0);

    let fee_rate = u64::MAX / tx_size + 1;
    let raw_product = u128::from(fee_rate) * u128::from(tx_size);
    assert!(raw_product > u128::from(u64::MAX));

    let expected_fee = u64::try_from(raw_product / 1_000).expect("final fee should fit");
    let actual_fee =
        checked_calculate_commitment_tx_fee(fee_rate, &None, CommitmentContractFeatures::LEGACY)
            .expect("intermediate u64 overflow should be allowed when final fee fits");

    assert_eq!(actual_fee, expected_fee);
}

#[test]
fn v1_commitment_reserve_accounts_for_the_extra_lock_arg_byte() {
    let legacy_occupied = occupied_capacity(
        &ckb_types::packed::Script::default(),
        &None,
        CommitmentContractFeatures::LEGACY,
    )
    .expect("Legacy occupied capacity should fit");
    let v1_occupied = occupied_capacity(
        &ckb_types::packed::Script::default(),
        &None,
        CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    )
    .expect("V1 occupied capacity should fit");
    let legacy_reserved = reserved_capacity(
        &ckb_types::packed::Script::default(),
        &None,
        CommitmentContractFeatures::LEGACY,
    )
    .expect("Legacy reserved capacity should fit");
    let v1_reserved = reserved_capacity(
        &ckb_types::packed::Script::default(),
        &None,
        CommitmentContractFeatures::ONCHAIN_FULL_PAYMENT_HASH,
    )
    .expect("V1 reserved capacity should fit");

    assert!(v1_occupied > legacy_occupied);
    assert!(v1_reserved > legacy_reserved);
}

#[test]
fn commitment_contract_features_determine_reserve() {
    use fiber_types::CommitmentContractFeatures as Features;

    let shutdown_script = ckb_types::packed::Script::default();
    let legacy = reserved_capacity(&shutdown_script, &None, Features::LEGACY).unwrap();
    let full_hash =
        reserved_capacity(&shutdown_script, &None, Features::ONCHAIN_FULL_PAYMENT_HASH).unwrap();
    assert!(full_hash > legacy);
}

#[test]
fn full_hash_feature_serializes_full_payment_hash_in_tlc() {
    use crate::fiber::channel::settlement_tlc_to_witness;
    use fiber_types::{CommitmentContractFeatures as Features, Hash256, HashAlgorithm, Privkey};

    let tlc = fiber_types::SettlementTlc {
        tlc_id: fiber_types::TLCId::Offered(0),
        hash_algorithm: HashAlgorithm::CkbHash,
        payment_amount: 1_000,
        payment_hash: Hash256::from([8u8; 32]),
        expiry: 0,
        local_key: Privkey::from(&[1u8; 32]),
        remote_key: Privkey::from(&[2u8; 32]).pubkey(),
    };
    let legacy = settlement_tlc_to_witness(&tlc, false, Features::LEGACY);
    let full_hash = settlement_tlc_to_witness(&tlc, false, Features::ONCHAIN_FULL_PAYMENT_HASH);
    assert_eq!(full_hash.len() - legacy.len(), 12);
    assert_eq!(&full_hash[17..49], tlc.payment_hash.as_ref());
}
