use bech32::ToBase32;
use ckb_hash::blake2b_256;
use ckb_types::packed::Script;
use secp256k1::{Keypair, Message, PublicKey, Secp256k1, SecretKey};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{
    fiber::{gen::invoice::RawCkbInvoice, types::Hash256},
    invoice::{
        invoice_impl::{CkbScript, InvoiceData, SIGNATURE_U5_SIZE},
        utils::{ar_decompress, ar_encompress, rand_sha256_hash},
        Attribute, CkbInvoice, Currency, InvoiceBuilder, InvoiceError, InvoiceSignature,
    },
};

fn gen_rand_public_key() -> PublicKey {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    PublicKey::from_keypair(&key_pair)
}

fn gen_rand_private_key() -> SecretKey {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    SecretKey::from_keypair(&key_pair)
}

fn gen_rand_keypair() -> (PublicKey, SecretKey) {
    let secp = Secp256k1::new();
    let key_pair = Keypair::new(&secp, &mut rand::thread_rng());
    (
        PublicKey::from_keypair(&key_pair),
        SecretKey::from_keypair(&key_pair),
    )
}

fn mock_invoice() -> CkbInvoice {
    let (public_key, private_key) = gen_rand_keypair();
    let mut invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(1280),
        signature: None,
        data: InvoiceData {
            payment_hash: rand_sha256_hash(),
            timestamp: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            attrs: vec![
                Attribute::FinalHtlcTimeout(5),
                Attribute::FinalHtlcMinimumCltvExpiry(12),
                Attribute::Description("description".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(1024)),
                Attribute::FallbackAddr("address".to_string()),
                Attribute::UdtScript(CkbScript(Script::default())),
                Attribute::PayeePublicKey(public_key),
            ],
        },
    };
    invoice
        .update_signature(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    invoice
}

#[test]
fn test_signature() {
    let private_key = gen_rand_private_key();
    let signature = Secp256k1::new().sign_ecdsa_recoverable(
        &Message::from_digest_slice(&[0u8; 32]).unwrap(),
        &private_key,
    );
    let signature = InvoiceSignature(signature);
    let base32 = signature.to_base32();
    assert_eq!(base32.len(), SIGNATURE_U5_SIZE);

    let decoded_signature = InvoiceSignature::from_base32(&base32).unwrap();
    assert_eq!(decoded_signature, signature);
}

#[test]
fn test_ckb_invoice() {
    let ckb_invoice = mock_invoice();
    let ckb_invoice_clone = ckb_invoice.clone();
    let raw_invoice: RawCkbInvoice = ckb_invoice.into();
    let decoded_invoice: CkbInvoice = raw_invoice.try_into().unwrap();
    assert_eq!(decoded_invoice, ckb_invoice_clone);
    let address = ckb_invoice_clone.to_string();
    assert!(address.starts_with("fibb1280"));
}

#[test]
fn test_invoice_bc32m() {
    let invoice = mock_invoice();
    assert!(invoice.is_signed());
    assert_eq!(invoice.check_signature(), Ok(()));

    let address = invoice.to_string();
    assert!(address.starts_with("fibb1280"));

    let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
    assert_eq!(decoded_invoice, invoice);
    assert!(decoded_invoice.is_signed());
    assert_eq!(decoded_invoice.amount(), Some(1280));
}

#[test]
fn test_invoice_from_str_err() {
    let invoice = mock_invoice();

    let address = invoice.to_string();
    assert!(address.starts_with("fibb1280"));

    let mut wrong = address.clone();
    wrong.push('1');
    let decoded_invoice = wrong.parse::<CkbInvoice>();
    assert_eq!(
        decoded_invoice.err(),
        Some(InvoiceError::Bech32Error(bech32::Error::InvalidLength))
    );

    let mut wrong = address.clone();
    // modify the values of wrong
    wrong.replace_range(10..12, "hi");
    let decoded_invoice = wrong.parse::<CkbInvoice>();
    assert_eq!(
        decoded_invoice.err(),
        Some(InvoiceError::Bech32Error(bech32::Error::InvalidChar('i')))
    );

    let mut wrong = address;
    // modify the values of wrong
    wrong.replace_range(10..12, "aa");
    let decoded_invoice = wrong.parse::<CkbInvoice>();
    assert_eq!(
        decoded_invoice.err(),
        Some(InvoiceError::Bech32Error(bech32::Error::InvalidChecksum))
    );

    wrong = wrong.replace("1280", "1281");
    let decoded_invoice = wrong.parse::<CkbInvoice>();
    assert_eq!(
        decoded_invoice.err(),
        Some(InvoiceError::Bech32Error(bech32::Error::InvalidChecksum))
    );
}

#[test]
fn test_invoice_bc32m_not_same() {
    let private_key = gen_rand_private_key();
    let signature = Secp256k1::new().sign_ecdsa_recoverable(
        &Message::from_digest_slice(&[0u8; 32]).unwrap(),
        &private_key,
    );
    let invoice = CkbInvoice {
        currency: Currency::Fibb,
        amount: Some(1280),
        signature: Some(InvoiceSignature(signature)),
        data: InvoiceData {
            payment_hash: [0u8; 32].into(),
            timestamp: 0,
            attrs: vec![
                Attribute::FinalHtlcTimeout(5),
                Attribute::FinalHtlcMinimumCltvExpiry(12),
                Attribute::Description("description hello".to_string()),
                Attribute::ExpiryTime(Duration::from_secs(1024)),
                Attribute::FallbackAddr("address".to_string()),
            ],
        },
    };

    let address = invoice.to_string();
    let decoded_invoice = address.parse::<CkbInvoice>().unwrap();
    assert_eq!(decoded_invoice, invoice);

    let mock_invoice = mock_invoice();
    let mock_address = mock_invoice.to_string();
    assert_ne!(mock_address, address);
}

#[test]
fn test_compress() {
    let input = "hrp1gyqsqqq5qqqqq9gqqqqp6qqqqq0qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq2qqqqqqqqqqqyvqsqqqsqqqqqvqqqqq8";
    let bytes = input.as_bytes();
    let compressed = ar_encompress(input.as_bytes()).unwrap();

    let decompressed = ar_decompress(&compressed).unwrap();
    let decompressed_str = std::str::from_utf8(&decompressed).unwrap();
    assert_eq!(input, decompressed_str);
    assert!(compressed.len() < bytes.len());
}

#[test]
fn test_invoice_builder() {
    let gen_payment_hash = rand_sha256_hash();
    let (public_key, private_key) = gen_rand_keypair();

    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .fallback_address("address".to_string())
        .expiry_time(Duration::from_secs(1024))
        .payee_pub_key(public_key)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
        .add_attr(Attribute::Description("description".to_string()))
        .add_attr(Attribute::UdtScript(CkbScript(Script::default())))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let address = invoice.to_string();

    assert_eq!(invoice, address.parse::<CkbInvoice>().unwrap());

    assert_eq!(invoice.currency, Currency::Fibb);
    assert_eq!(invoice.amount, Some(1280));
    assert_eq!(invoice.payment_hash(), &gen_payment_hash);
    assert_eq!(invoice.data.attrs.len(), 7);
    assert_eq!(invoice.check_signature().is_ok(), true);
}

#[test]
fn test_invoice_check_signature() {
    let gen_payment_hash = rand_sha256_hash();
    let (public_key, private_key) = gen_rand_keypair();

    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .fallback_address("address".to_string())
        .expiry_time(Duration::from_secs(1024))
        .payee_pub_key(public_key)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
        .add_attr(Attribute::Description("description".to_string()))
        .add_attr(Attribute::UdtScript(CkbScript(Script::default())))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    assert_eq!(invoice.check_signature(), Ok(()));
    let payee_pubkey = invoice.payee_pub_key();
    assert_eq!(payee_pubkey, Some(&public_key));

    // modify the some element then check signature will fail
    let mut invoice_clone = invoice.clone();
    invoice_clone.data.attrs[0] = Attribute::FinalHtlcTimeout(6);
    assert_eq!(
        invoice_clone.check_signature(),
        Err(InvoiceError::InvalidSignature)
    );

    let mut invoice_clone = invoice.clone();
    invoice_clone.amount = Some(1281);
    assert_eq!(
        invoice_clone.check_signature(),
        Err(InvoiceError::InvalidSignature)
    );

    // if the invoice is not signed, check_signature will skipped
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .fallback_address("address".to_string())
        .expiry_time(Duration::from_secs(1024))
        .payee_pub_key(public_key)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
        .build()
        .unwrap();

    assert_eq!(invoice.check_signature(), Ok(()));
    // modify the some element then check signature will also skip
    let mut invoice_clone = invoice.clone();
    invoice_clone.amount = Some(1281);
    assert_eq!(invoice_clone.check_signature(), Ok(()));
}

#[test]
fn test_invoice_signature_check() {
    let gen_payment_hash = rand_sha256_hash();
    let (_, private_key) = gen_rand_keypair();
    let public_key = gen_rand_public_key();

    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .fallback_address("address".to_string())
        .expiry_time(Duration::from_secs(1024))
        .payee_pub_key(public_key)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcMinimumCltvExpiry(12))
        .add_attr(Attribute::Description("description".to_string()))
        .add_attr(Attribute::UdtScript(CkbScript(Script::default())))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

    assert_eq!(invoice.err(), Some(InvoiceError::InvalidSignature));
}

#[test]
fn test_invoice_builder_duplicated_attr() {
    let gen_payment_hash = rand_sha256_hash();
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(gen_payment_hash)
        .add_attr(Attribute::FinalHtlcTimeout(5))
        .add_attr(Attribute::FinalHtlcTimeout(6))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

    assert_eq!(
        invoice.err(),
        Some(InvoiceError::DuplicatedAttributeKey(format!(
            "{:?}",
            Attribute::FinalHtlcTimeout(5)
        )))
    );
}

#[test]
fn test_invoice_builder_missing() {
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(rand_sha256_hash())
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

    assert_eq!(invoice.err(), None);

    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(rand_sha256_hash())
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

    assert_eq!(invoice.err(), None);
}

#[test]
fn test_invoice_builder_preimage() {
    let preimage = rand_sha256_hash();
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(preimage)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    let clone_invoice = invoice.clone();
    assert_eq!(hex::encode(invoice.payment_hash()).len(), 64);

    let raw_invoice: RawCkbInvoice = invoice.into();
    let decoded_invoice: CkbInvoice = raw_invoice.try_into().unwrap();
    assert_eq!(decoded_invoice, clone_invoice);
}

#[test]
fn test_invoice_builder_both_payment_hash_preimage() {
    let preimage = rand_sha256_hash();
    let payment_hash = rand_sha256_hash();
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(payment_hash)
        .payment_preimage(preimage)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));

    assert_eq!(
        invoice.err(),
        Some(InvoiceError::BothPaymenthashAndPreimage)
    );
}

#[test]
fn test_invoice_serialize() {
    let invoice = mock_invoice();
    let res = serde_json::to_string(&invoice);
    assert!(res.is_ok());
    let decoded = serde_json::from_str::<CkbInvoice>(&res.unwrap()).unwrap();
    assert_eq!(decoded, invoice);
}

#[test]
fn test_invoice_timestamp() {
    let payment_hash = rand_sha256_hash();
    let private_key = gen_rand_private_key();
    let invoice1 = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(payment_hash)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    let invoice2 = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(payment_hash)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    assert_ne!(invoice1.data.timestamp, invoice2.data.timestamp);
    assert_ne!(invoice1.to_string(), invoice2.to_string());
}

#[test]
fn test_invoice_gen_payment_hash() {
    let private_key = gen_rand_private_key();
    let payment_preimage = rand_sha256_hash();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_preimage(payment_preimage)
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    let payment_hash = invoice.payment_hash();
    let expected_hash: Hash256 = blake2b_256(payment_preimage.as_ref()).into();
    assert_eq!(expected_hash, *payment_hash);
}

#[test]
fn test_invoice_rand_payment_hash() {
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key));
    assert!(invoice.is_ok());
}

#[test]
fn test_invoice_udt_script() {
    let script = Script::default();
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(rand_sha256_hash())
        .udt_type_script(script.clone())
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();
    assert_eq!(invoice.udt_type_script().unwrap(), &script);

    let res = serde_json::to_string(&invoice);
    assert!(res.is_ok());
    let decoded = serde_json::from_str::<CkbInvoice>(&res.unwrap()).unwrap();
    assert_eq!(decoded, invoice);
}

#[test]
fn test_invoice_check_expired() {
    let private_key = gen_rand_private_key();
    let invoice = InvoiceBuilder::new(Currency::Fibb)
        .amount(Some(1280))
        .payment_hash(rand_sha256_hash())
        .expiry_time(Duration::from_secs(1))
        .build_with_sign(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &private_key))
        .unwrap();

    assert_eq!(invoice.is_expired(), false);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(invoice.is_expired(), true);
}
