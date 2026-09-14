use super::*;

/// Plain decode of one input slice, for tests that assert the log view.
/// Production code uses [`DecodedStream::consume_chunk`] instead.
fn consume(stream: &mut DecodedStream, bytes: &[u8]) -> Vec<Vec<u8>> {
    stream
        .consume_chunk(bytes, false)
        .lines
        .into_iter()
        .map(|line| line.log)
        .collect()
}

/// Plain decode plus the VT-styled form of each completed line.
fn consume_styled(stream: &mut DecodedStream, bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    stream.raw.consume(bytes);
    stream.consume_inner(bytes, true)
}

#[test]
fn terminal_stream_handles_split_utf8_and_multiple_chunks() {
    let mut stream = DecodedStream::new();
    let bytes = "ż界\n".as_bytes();

    assert!(consume(&mut stream, &bytes[..1]).is_empty());
    assert!(consume(&mut stream, &bytes[1..3]).is_empty());
    assert_eq!(consume(&mut stream, &bytes[3..]), ["ż界\n".as_bytes()]);
}

#[test]
fn terminal_stream_preserves_meaningful_spaces_and_wide_cursor_position() {
    let mut stream = DecodedStream::new();

    consume(&mut stream, "界  \x1b[D".as_bytes());

    assert_eq!(stream.visible_line(), "界  ".as_bytes());
    assert_eq!(stream.cursor_column(), 3);
}

#[test]
fn terminal_stream_bounds_ascii_after_the_column_cap() {
    let mut stream = DecodedStream::new();
    let input = vec![b'a'; MAX_TERMINAL_COLUMNS * 16];

    consume(&mut stream, &input);

    assert_eq!(stream.parser.screen().size(), (1, TERMINAL_BACKING_COLUMNS));
    assert_eq!(stream.visible_line(), vec![b'a'; MAX_TERMINAL_COLUMNS]);
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);
}

#[test]
fn terminal_stream_handles_wide_characters_at_and_after_the_boundary() {
    let mut stream = DecodedStream::new();
    consume(&mut stream, &vec![b'a'; MAX_TERMINAL_COLUMNS - 2]);

    consume(&mut stream, "界界".as_bytes());

    let mut expected = vec![b'a'; MAX_TERMINAL_COLUMNS - 2];
    expected.extend_from_slice("界".as_bytes());
    assert_eq!(stream.visible_line(), expected);
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);
}

#[test]
fn terminal_stream_keeps_combining_character_on_the_boundary_cell() {
    let mut stream = DecodedStream::new();
    consume(&mut stream, &vec![b'a'; MAX_TERMINAL_COLUMNS - 1]);

    consume(&mut stream, "e\u{301}".as_bytes());

    let mut expected = vec![b'a'; MAX_TERMINAL_COLUMNS - 1];
    expected.extend_from_slice("e\u{301}".as_bytes());
    assert_eq!(stream.visible_line(), expected);
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);
}

#[test]
fn terminal_stream_caps_cursor_movement_to_the_visible_width() {
    let mut stream = DecodedStream::new();

    consume(&mut stream, b"abc\x1b[9999C");
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);

    consume(&mut stream, b"\x1b[2DX");
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS - 1);
}

#[test]
fn styled_completions_keep_shell_colors_and_reset_afterwards() {
    let mut stream = DecodedStream::new();

    let completed = consume_styled(&mut stream, b"\x1b[32mgreen\nnext");

    assert_eq!(
        completed,
        [(b"green\n".to_vec(), b"\x1b[32mgreen\x1b[0m".to_vec())]
    );
    assert_eq!(stream.visible_line(), b"next");
}

#[test]
fn terminal_stream_collapses_redraws() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(
            &mut stream,
            b"\r\x1b[2K> help\r\x1b[2K> \r\x1b[2K> help\r\n"
        ),
        [b"> help\n"]
    );
}

#[test]
fn terminal_stream_strips_sgr() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"\x1b[32mgreen \x1b[31mred\x1b[0m\n"),
        [b"green red\n"]
    );
}

#[test]
fn terminal_stream_handles_escape_sequences_split_between_reads() {
    let mut stream = DecodedStream::new();

    assert!(consume(&mut stream, b"old\r\x1b[").is_empty());
    assert_eq!(consume(&mut stream, b"2Knew\n"), [b"new\n"]);
}

#[test]
fn terminal_stream_overwrites_without_truncating_the_tail() {
    let mut stream = DecodedStream::new();

    assert_eq!(consume(&mut stream, b"abc\x1b[1GX\n"), [b"Xbc\n"]);
}

#[test]
fn terminal_stream_keeps_tail_when_tab_moves_cursor_backwards() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"abcdefghij\x1b[3G\t\n"),
        [b"abcdefghij\n"]
    );
}

#[test]
fn terminal_stream_keeps_combining_marks_with_their_base_character() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, "e\u{301}\n".as_bytes()),
        ["e\u{301}\n".as_bytes()]
    );
}

#[test]
fn terminal_stream_erases_the_cursor_cell_with_csi_one_k() {
    let mut stream = DecodedStream::new();

    assert_eq!(consume(&mut stream, b"abc\x1b[1K\n"), [b"\n"]);
}

#[test]
fn terminal_stream_handles_zephyr_erase_display_after_backspace() {
    let mut stream = DecodedStream::new();

    assert_eq!(
        consume(&mut stream, b"rtt:~$ abc\x1b[1D\x1b[J\r\n"),
        [b"rtt:~$ ab\n"]
    );
}

#[test]
fn terminal_stream_deletes_characters_with_csi_p() {
    let mut stream = DecodedStream::new();

    assert_eq!(consume(&mut stream, b"abc\x1b[D\x1b[P\n"), [b"ab\n"]);
}

#[test]
fn chunk_uses_styled_display_for_complex_lines_when_interactive() {
    let mut stream = DecodedStream::new();

    let interactive = stream.consume_chunk(b"abcdef\rxy\n", true);
    assert_eq!(interactive.lines.len(), 1);
    assert_eq!(interactive.lines[0].log, b"xycdef\n");
    assert!(!interactive.lines[0].display.is_empty());

    let mut redirected_stream = DecodedStream::new();
    let redirected = redirected_stream.consume_chunk(b"abcdef\rxy\n", false);
    assert_eq!(redirected.lines.len(), 1);
    assert_eq!(redirected.lines[0].log, b"xycdef\n");
    assert_eq!(redirected.lines[0].display, b"xycdef");
}

#[test]
fn chunk_partial_uses_terminal_rendering_for_complex_input() {
    let mut stream = DecodedStream::new();

    let chunk = stream.consume_chunk(b"> abc\x1b[D\x1b[P", true);
    assert!(chunk.lines.is_empty());
    // Plain log collapses the delete; overlay keeps styling and cursor state.
    assert_eq!(chunk.partial.log, b"> ab");
    assert_eq!(chunk.partial.display, b"> ab");
    assert!(chunk.partial.overlay.starts_with(b"\x1b7"));
}
