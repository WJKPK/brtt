use super::*;

#[test]
fn interactive_diagnostics_clear_the_current_line() {
    let mut output = Vec::new();

    write_diagnostic_line(
        &mut output,
        log::Level::Info,
        format_args!("down channel temporarily unavailable"),
        true,
    )
    .unwrap();

    assert_eq!(
        output,
        b"\r\x1b[2K[brtt INFO] down channel temporarily unavailable\r\n"
    );
}
