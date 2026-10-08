use super::*;

#[test]
fn selection_defaults_only_when_single_probe_exists() {
    assert_eq!(automatic_selection(1, None).unwrap(), Some(0));
    assert_eq!(automatic_selection(2, None).unwrap(), None);
}

#[test]
fn explicit_probe_zero_is_not_treated_as_missing() {
    assert_eq!(
        automatic_selection(2, Some(&ProbeInfo::Number(0))).unwrap(),
        Some(0)
    );
}

#[test]
fn probe_listing_returns_writer_errors() {
    struct BrokenPipe;

    impl Write for BrokenPipe {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let error = list_probes(BrokenPipe, &[]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
}
