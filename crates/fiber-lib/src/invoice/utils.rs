// These are used internally in other modules and tests.
#[allow(unused_imports)]
pub(crate) use fiber_types::{
    ar_decompress, ar_encompress, construct_invoice_preimage, parse_hrp, SIGNATURE_U5_SIZE,
};

#[cfg(test)]
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn test_parse_hrp() {
    use super::invoice_impl::Currency;
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
