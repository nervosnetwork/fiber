use super::*;

#[test]
fn validate_watchtower_quotas() {
    let base = LspConfig::default();
    assert!(base.validate().is_ok());

    let mut zero_per_tenant_ch = base.clone();
    zero_per_tenant_ch.max_watch_channels_per_tenant = 0;
    assert!(zero_per_tenant_ch.validate().is_err());

    let mut zero_total_ch = base.clone();
    zero_total_ch.max_watch_channels = 0;
    assert!(zero_total_ch.validate().is_err());

    let mut inverted_ch = base.clone();
    inverted_ch.max_watch_channels_per_tenant = inverted_ch.max_watch_channels + 1;
    assert!(inverted_ch.validate().is_err());

    let mut zero_per_tenant_pi = base.clone();
    zero_per_tenant_pi.max_watch_preimages_per_tenant = 0;
    assert!(zero_per_tenant_pi.validate().is_err());

    let mut zero_total_pi = base.clone();
    zero_total_pi.max_watch_preimages = 0;
    assert!(zero_total_pi.validate().is_err());

    let mut inverted_pi = base.clone();
    inverted_pi.max_watch_preimages_per_tenant = inverted_pi.max_watch_preimages + 1;
    assert!(inverted_pi.validate().is_err());
}
