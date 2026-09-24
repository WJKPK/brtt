use super::*;

#[test]
fn scan_region_prefers_elf_symbol_then_explicit_then_default() {
    let target_default = ScanRegion::Exact(0x3000);

    let discovery = resolve_scan_region(
        Some(&ScanRegion::Exact(0x1000)),
        Some(&ScanRegion::Exact(0x2000)),
        &target_default,
    );
    assert!(matches!(
        discovery,
        RttDiscovery::Fixed(ScanRegion::Exact(0x1000))
    ));

    let discovery = resolve_scan_region(
        None,
        Some(&ScanRegion::range(0x1000..0x2000)),
        &target_default,
    );
    assert!(matches!(
        discovery,
        RttDiscovery::Fixed(ScanRegion::Ranges(_))
    ));

    let discovery = resolve_scan_region(None, None, &target_default);
    assert!(matches!(
        discovery,
        RttDiscovery::Incremental(ScanRegion::Exact(0x3000))
    ));
}

#[test]
fn cleanup_error_never_masks_the_primary_error() {
    let error = digest_result(
        Err(anyhow::anyhow!("loop failed")),
        Err(anyhow::anyhow!("flush failed")),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("loop failed"), "{error}");
    assert!(error.contains("flush failed"), "{error}");

    let error = digest_result(Ok(()), Err(anyhow::anyhow!("flush failed"))).unwrap_err();
    assert_eq!(error.to_string(), "flush failed");
}

#[test]
fn core_inputs_sort_elfs_and_supply_default_core() {
    let inputs = core_inputs(vec![
        (
            2,
            ElfContents {
                region: Some(ScanRegion::Exact(0x2000)),
                defmt: None,
            },
        ),
        (
            1,
            ElfContents {
                region: Some(ScanRegion::Exact(0x1000)),
                defmt: None,
            },
        ),
    ]);
    assert_eq!(
        inputs.iter().map(|input| input.id).collect::<Vec<_>>(),
        vec![CoreId::new(1), CoreId::new(2)]
    );

    let default = core_inputs(Vec::new());
    assert_eq!(default.len(), 1);
    assert_eq!(default[0].id, CoreId::new(0));
    assert!(default[0].elf_region.is_none());

    let without_symbol = core_inputs(vec![(
        3,
        ElfContents {
            region: None,
            defmt: None,
        },
    )]);
    assert!(matches!(
        resolve_scan_region(
            without_symbol[0].elf_region.as_ref(),
            Some(&ScanRegion::Exact(0x4000)),
            &ScanRegion::Exact(0x5000)
        ),
        RttDiscovery::Fixed(ScanRegion::Exact(0x4000))
    ));
}
