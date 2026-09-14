use super::*;

#[test]
fn scan_region_prefers_elf_symbol_over_explicit_region() {
    let elf = ScanRegion::Exact(0x1000);
    let requested = ScanRegion::Exact(0x2000);
    let target_default = ScanRegion::Exact(0x3000);

    let discovery = resolve_scan_region(Some(&elf), Some(&requested), &target_default);

    assert!(matches!(
        discovery,
        RttDiscovery::Fixed(ScanRegion::Exact(0x1000))
    ));
}

#[test]
fn scan_region_uses_explicit_region_when_elf_absent() {
    let requested = ScanRegion::range(0x1000..0x2000);
    let target_default = ScanRegion::Exact(0x3000);

    let discovery = resolve_scan_region(None, Some(&requested), &target_default);

    assert!(matches!(
        discovery,
        RttDiscovery::Fixed(ScanRegion::Ranges(_))
    ));
}

#[test]
fn scan_region_falls_back_to_target_default_with_automatic_scan() {
    let target_default = ScanRegion::Exact(0x3000);

    let discovery = resolve_scan_region(None, None, &target_default);

    assert!(matches!(
        discovery,
        RttDiscovery::Incremental(ScanRegion::Exact(0x3000))
    ));
}

#[test]
fn cleanup_error_does_not_mask_primary_error() {
    let error = digest_result(
        Err(anyhow::anyhow!("loop failed")),
        Err(anyhow::anyhow!("flush failed")),
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("loop failed"), "{error}");
    assert!(error.contains("flush failed"), "{error}");
}

#[test]
fn cleanup_error_surfaces_when_loop_succeeded() {
    let error = digest_result(Ok(()), Err(anyhow::anyhow!("flush failed"))).unwrap_err();

    assert_eq!(error.to_string(), "flush failed");
}
