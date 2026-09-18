use super::*;

#[test]
fn interactive_diagnostics_reset_terminal_attributes() {
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
        b"\r\x1b[2K\x1b[0m[brtt INFO] down channel temporarily unavailable\x1b[0m\r\n"
    );
}

#[test]
fn redirected_diagnostics_have_no_terminal_controls() {
    let mut output = Vec::new();

    write_diagnostic_line(
        &mut output,
        log::Level::Info,
        format_args!("target reset issued"),
        false,
    )
    .unwrap();

    assert_eq!(output, b"[brtt INFO] target reset issued\n");
}
