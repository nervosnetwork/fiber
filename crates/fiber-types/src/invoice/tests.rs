use super::*;

#[test]
fn invoice_without_final_expiry_delta_uses_protocol_default() {
    let now = crate::now_timestamp_as_millis_u64();
    let invoice = CkbInvoice {
        currency: Currency::Fibd,
        amount: Some(1_000),
        signature: None,
        data: InvoiceData {
            timestamp: now.into(),
            payment_hash: Hash256::default(),
            attrs: Vec::new(),
        },
    };

    assert_eq!(
        invoice.final_tlc_minimum_expiry_delta_or_default(),
        DEFAULT_FINAL_TLC_EXPIRY_DELTA
    );
    assert!(invoice.is_tlc_expire_too_soon(now + 12 * 60 * 60 * 1_000));
    assert!(!invoice.is_tlc_expire_too_soon(now + 48 * 60 * 60 * 1_000));

    let explicit_delta = 36 * 60 * 60 * 1_000;
    let mut invoice_with_explicit_delta = invoice;
    invoice_with_explicit_delta
        .data
        .attrs
        .push(Attribute::FinalHtlcMinimumExpiryDelta(explicit_delta));
    assert_eq!(
        invoice_with_explicit_delta.final_tlc_minimum_expiry_delta_or_default(),
        explicit_delta
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_parse_hrp() {
    use super::InvoiceError;

    let res = parse_hrp("fibb1280");
    assert_eq!(res, Ok((Currency::Fibb, Some(1280))));

    let res = parse_hrp("fibb");
    assert_eq!(res, Ok((Currency::Fibb, None)));

    let res = parse_hrp("fibt1023");
    assert_eq!(res, Ok((Currency::Fibt, Some(1023))));

    let res = parse_hrp("fibt10");
    assert_eq!(res, Ok((Currency::Fibt, Some(10))));

    let res = parse_hrp("fibt");
    assert_eq!(res, Ok((Currency::Fibt, None)));

    let res = parse_hrp("xnfibb");
    assert_eq!(res, Err(InvoiceError::MalformedHRP("xnfibb".to_string())));

    let res = parse_hrp("lxfibt");
    assert_eq!(res, Err(InvoiceError::MalformedHRP("lxfibt".to_string())));

    let res = parse_hrp("fibt");
    assert_eq!(res, Ok((Currency::Fibt, None)));

    let res = parse_hrp("fixt");
    assert_eq!(res, Err(InvoiceError::MalformedHRP("fixt".to_string())));

    let res = parse_hrp("fibtt");
    assert_eq!(
        res,
        Err(InvoiceError::MalformedHRP(
            "fibtt, unexpected ending `t`".to_string()
        ))
    );

    let res = parse_hrp("fibt1x24");
    assert_eq!(
        res,
        Err(InvoiceError::MalformedHRP(
            "fibt1x24, unexpected ending `x24`".to_string()
        ))
    );

    let res = parse_hrp("fibt000");
    assert_eq!(res, Ok((Currency::Fibt, Some(0))));

    let res = parse_hrp("fibt1024444444444444444444444444444444444444444444444444444444444444");
    assert!(matches!(res, Err(InvoiceError::ParseAmountError(_))));

    let res = parse_hrp("fibt0x");
    assert!(matches!(res, Err(InvoiceError::MalformedHRP(_))));

    let res = parse_hrp("");
    assert!(matches!(res, Err(InvoiceError::MalformedHRP(_))));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_compress() {
    let input = "hrp1gyqsqqq5qqqqq9gqqqqp6qqqqq0qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq2qqqqqqqqqqqyvqsqqqsqqqqqvqqqqq8";
    let bytes = input.as_bytes();
    let compressed = ar_encompress(input.as_bytes()).unwrap();

    let decompressed = ar_decompress_with_limit(&compressed, MAX_INVOICE_DATA_LENGTH).unwrap();
    let decompressed_str = std::str::from_utf8(&decompressed).unwrap();
    assert_eq!(input, decompressed_str);
    assert!(compressed.len() < bytes.len());
}

fn raw_invoice_data_with_attrs(attrs: Vec<InvoiceAttr>) -> gen_invoice::RawInvoiceData {
    RawInvoiceDataBuilder::default()
        .timestamp(0u128.pack())
        .payment_hash(PaymentHash::new_builder().set([Byte::new(0); 32]).build())
        .attrs(InvoiceAttrsVec::new_builder().set(attrs).build())
        .build()
}

fn encode_unsigned_invoice(raw_invoice_data: gen_invoice::RawInvoiceData) -> String {
    let compressed = ar_encompress(raw_invoice_data.as_slice()).unwrap();
    let mut data = vec![u5::try_from_u8(0).unwrap()];
    data.extend(compressed.to_base32());
    assert!(data.len() >= SIGNATURE_U5_SIZE);
    encode("fibb", data, Variant::Bech32m).unwrap()
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_parse_malformed_compressed_invoice_returns_error_without_panic() {
    let mut data = vec![u5::try_from_u8(0).unwrap()];
    data.extend(std::iter::repeat_n(
        u5::try_from_u8(31).unwrap(),
        SIGNATURE_U5_SIZE,
    ));
    let invoice = encode("fibb", data, Variant::Bech32m).unwrap();

    let result = std::panic::catch_unwind(|| CkbInvoice::from_str_allowing_unsigned(&invoice));

    assert!(result.is_ok());
    assert!(result.unwrap().is_err());
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_decompressed_invoice_data_length_is_limited() {
    let payload = vec![0u8; MAX_INVOICE_DATA_LENGTH + 1];
    let compressed = ar_encompress(&payload).unwrap();

    let result = ar_decompress_with_limit(&compressed, MAX_INVOICE_DATA_LENGTH);

    assert!(matches!(
        result,
        Err(InvoiceError::InvoiceDataTooLong {
            len,
            max: MAX_INVOICE_DATA_LENGTH,
        }) if len > MAX_INVOICE_DATA_LENGTH
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_malformed_text_attribute_returns_error_without_panic() {
    let attr = InvoiceAttr::new_builder()
        .set(InvoiceAttrUnion::Description(
            Description::new_builder().value([0xff; 200].pack()).build(),
        ))
        .build();
    let invoice = encode_unsigned_invoice(raw_invoice_data_with_attrs(vec![attr]));

    let result = std::panic::catch_unwind(|| CkbInvoice::from_str_allowing_unsigned(&invoice));

    assert!(matches!(
        result,
        Ok(Err(InvoiceError::InvalidUtf8Attribute("description")))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_malformed_payee_public_key_returns_error_without_panic() {
    let attr = InvoiceAttr::new_builder()
        .set(InvoiceAttrUnion::PayeePublicKey(
            PayeePublicKey::new_builder()
                .value([1, 2, 3].pack())
                .build(),
        ))
        .build();
    let raw_invoice_data = raw_invoice_data_with_attrs(vec![attr]);

    let result = std::panic::catch_unwind(|| InvoiceData::try_from(raw_invoice_data));

    assert!(matches!(
        result,
        Ok(Err(InvoiceError::InvalidPayeePublicKey))
    ));
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_malformed_raw_invoice_signature_returns_error_without_panic() {
    let signature = gen_invoice::Signature::new_builder()
        .set([Byte::new(32); SIGNATURE_U5_SIZE])
        .build();
    let raw_invoice = gen_invoice::RawCkbInvoiceBuilder::default()
        .currency(Byte::new(Currency::Fibb as u8))
        .signature(
            gen_invoice::SignatureOpt::new_builder()
                .set(Some(signature))
                .build(),
        )
        .data(raw_invoice_data_with_attrs(vec![]))
        .build();

    let result = std::panic::catch_unwind(|| CkbInvoice::try_from(raw_invoice));

    assert!(matches!(
        result,
        Ok(Err(InvoiceError::InvalidSignatureEncoding))
    ));
}
