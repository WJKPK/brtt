use super::*;

/// Decode completed lines through the production entry point and return their
/// plain representation.
fn consume(stream: &mut DecodedStream, bytes: &[u8]) -> Vec<Vec<u8>> {
    stream
        .consume_chunk(bytes, false)
        .lines
        .into_iter()
        .map(|line| line.plain)
        .collect()
}

/// Decode completed lines through the production entry point and return both
/// their plain and styled representations.
fn consume_styled(stream: &mut DecodedStream, bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    stream
        .consume_chunk(bytes, true)
        .lines
        .into_iter()
        .map(|line| (line.plain, line.styled))
        .collect()
}

/// The decoded result must not depend on where the transport happened to split
/// the byte stream.
fn assert_same_at_every_split(input: &[u8], expected: &[u8]) {
    for split in 0..=input.len() {
        let mut stream = DecodedStream::new();

        let mut lines = consume(&mut stream, &input[..split]);
        lines.extend(consume(&mut stream, &input[split..]));

        assert_eq!(
            lines,
            vec![expected.to_vec()],
            "different result when input was split at byte {split}"
        );
    }
}

#[test]
fn utf8_is_chunk_independent() {
    assert_same_at_every_split("ż界\n".as_bytes(), "ż界\n".as_bytes());
}

#[test]
fn csi_is_chunk_independent() {
    assert_same_at_every_split(b"abc\x1b[2DX\n", b"aXc\n");
}

#[test]
fn redraw_is_chunk_independent() {
    assert_same_at_every_split(b"old\r\x1b[2Knew\n", b"new\n");
}

#[test]
fn crlf_is_chunk_independent() {
    assert_same_at_every_split(b"hello\r\n", b"hello\n");
}

#[test]
fn sgr_is_removed_from_plain_output() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"\x1b[32mgreen \x1b[31mred\x1b[0m\n"),
        vec![b"green red\n".to_vec()]
    );
}

#[test]
fn styled_output_preserves_attributes_without_reset() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume_styled(&mut stream, b"\x1b[32mgreen\n"),
        vec![(b"green\n".to_vec(), b"\x1b[32mgreen".to_vec(),)]
    );
}

#[test]
fn styled_plain_line_leaves_reset_to_renderer() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume_styled(&mut stream, b"plain\n"),
        vec![(b"plain\n".to_vec(), b"plain".to_vec(),)]
    );
}

#[test]
fn carriage_return_redraw_collapses_to_final_line() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(
            &mut stream,
            b"\r\x1b[2K> help\r\x1b[2K> \r\x1b[2K> help\r\n",
        ),
        vec![b"> help\n".to_vec()]
    );
}

#[test]
fn cursor_overwrite_preserves_unmodified_tail() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"abc\x1b[1GX\n"),
        vec![b"Xbc\n".to_vec()]
    );
}

#[test]
fn tab_cursor_motion_does_not_truncate_tail() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"abcdefghij\x1b[3G\t\n"),
        vec![b"abcdefghij\n".to_vec()]
    );
}

#[test]
fn combining_mark_stays_with_base_character() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, "e\u{301}\n".as_bytes()),
        vec!["e\u{301}\n".as_bytes().to_vec()]
    );
}

#[test]
fn csi_one_k_erases_through_cursor() {
    let mut stream = DecodedStream::new();

    assert_eq!(consume(&mut stream, b"abc\x1b[1K\n"), vec![b"\n".to_vec()]);
}

#[test]
fn erase_display_after_backspace_matches_zephyr_prompt_behavior() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"rtt:~$ abc\x1b[1D\x1b[J\r\n"),
        vec![b"rtt:~$ ab\n".to_vec()]
    );
}

#[test]
fn csi_p_deletes_character_at_cursor() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"abc\x1b[D\x1b[P\n"),
        vec![b"ab\n".to_vec()]
    );
}

#[test]
fn carriage_return_overwrite_preserves_existing_tail() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"abcdef\rxy\n"),
        vec![b"xycdef\n".to_vec()]
    );
}

#[test]
fn osc_payload_does_not_create_logical_lines() {
    let mut stream = DecodedStream::new();

    assert!(stream
        .consume_chunk(b"\x1b]some\npayload", false)
        .lines
        .is_empty());
    assert!(stream.consume_chunk(b"\x07", false).lines.is_empty());

    assert_eq!(consume(&mut stream, b"text\n"), vec![b"text\n".to_vec()]);
}

#[test]
fn carriage_return_inside_osc_is_not_a_line_boundary() {
    let mut stream = DecodedStream::new();

    assert!(stream
        .consume_chunk(b"\x1b]foo\rbar\x07", false)
        .lines
        .is_empty());

    assert_eq!(
        consume(&mut stream, b"visible\n"),
        vec![b"visible\n".to_vec()]
    );
}

#[test]
fn osc_with_bel_is_chunk_independent() {
    assert_same_at_every_split(b"\x1b]title\nignored\x07visible\n", b"visible\n");
}

#[test]
fn osc_with_st_is_chunk_independent() {
    assert_same_at_every_split(b"\x1b]title\nignored\x1b\\visible\n", b"visible\n");
}

#[test]
fn dcs_with_st_is_chunk_independent() {
    assert_same_at_every_split(b"\x1bPpayload\nignored\x1b\\visible\n", b"visible\n");
}

#[test]
fn overflow_does_not_affect_edits_at_visible_boundary() {
    let mut stream = DecodedStream::new();

    let mut input = vec![b'a'; MAX_TERMINAL_COLUMNS * 2];
    input.extend_from_slice(b"\x1b[2DX\n");

    let lines = consume(&mut stream, &input);

    let mut expected = vec![b'a'; MAX_TERMINAL_COLUMNS];
    expected[MAX_TERMINAL_COLUMNS - 2] = b'X';
    expected.push(b'\n');

    assert_eq!(lines, vec![expected]);
}

#[test]
fn completed_line_starts_fresh_terminal_row() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"first\nsecond\n"),
        vec![b"first\n".to_vec(), b"second\n".to_vec(),]
    );
}

#[test]
fn attributes_survive_across_logical_lines_until_changed() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume_styled(&mut stream, b"\x1b[32mgreen\nstill green\n"),
        vec![
            (b"green\n".to_vec(), b"\x1b[32mgreen".to_vec(),),
            (b"still green\n".to_vec(), b"\x1b[32mstill green".to_vec(),),
        ]
    );
}

fn assert_same_at_every_split_lines(input: &[u8], expected: &[&[u8]]) {
    let expected: Vec<Vec<u8>> = expected.iter().map(|line| line.to_vec()).collect();
    for split in 0..=input.len() {
        let mut stream = DecodedStream::new();

        let mut lines = consume(&mut stream, &input[..split]);
        lines.extend(consume(&mut stream, &input[split..]));

        assert_eq!(
            lines, expected,
            "different result when input was split at byte {split}"
        );
    }
}

#[test]
fn cyrillic_utf8_is_chunk_independent() {
    assert_same_at_every_split("Н\n".as_bytes(), "Н\n".as_bytes());
}

#[test]
fn sos_payload_does_not_create_logical_lines() {
    assert_same_at_every_split(b"\x1bXfoo\nbar\x1b\\visible\n", b"visible\n");
}

#[test]
fn osc_escape_recovers_into_csi() {
    assert_same_at_every_split(b"\x1b]foo\x1b[31mred\n", b"red\n");
}

#[test]
fn lf_inside_csi_is_a_logical_boundary() {
    assert_same_at_every_split_lines(b"\x1b[\nX\n", &[b"\n", b"\n"]);
}

#[test]
fn c0_inside_escape_preserves_escape_detection() {
    assert_same_at_every_split(b"\x1b\x07]ignored\npayload\x07visible\n", b"visible\n");
}

#[test]
fn cursor_clamp_does_not_cancel_pending_csi() {
    let mut input = vec![b'a'; MAX_TERMINAL_COLUMNS];
    input.extend_from_slice(b"\x1b[\tHX\n");

    let lines = consume(&mut DecodedStream::new(), &input);

    let mut expected = vec![b'a'; MAX_TERMINAL_COLUMNS];
    expected[0] = b'X';
    expected.push(b'\n');
    assert_eq!(lines, vec![expected]);
}
