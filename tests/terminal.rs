use super::*;

#[test]
fn terminal_stream_preserves_sgr_across_newlines() {
    let mut stream = DecodedStream::new();

    assert_eq!(stream.consume(b"\x1b[32mgreen\nnext"), [b"green\n"]);
    assert_eq!(stream.visible_line(), b"next");
    assert!(stream.styled_visible_line().starts_with(b"\x1b[32m"));
}

#[test]
fn terminal_stream_handles_split_utf8_and_multiple_chunks() {
    let mut stream = DecodedStream::new();
    let bytes = "ż界\n".as_bytes();

    assert!(stream.consume(&bytes[..1]).is_empty());
    assert!(stream.consume(&bytes[1..3]).is_empty());
    assert_eq!(stream.consume(&bytes[3..]), ["ż界\n".as_bytes()]);
}

#[test]
fn terminal_stream_preserves_meaningful_spaces_and_wide_cursor_position() {
    let mut stream = DecodedStream::new();

    stream.consume("界  \x1b[D".as_bytes());

    assert_eq!(stream.visible_line(), "界  ".as_bytes());
    assert_eq!(stream.cursor_back_from_end(), 1);
}

#[test]
fn terminal_stream_bounds_ascii_after_the_column_cap() {
    let mut stream = DecodedStream::new();
    let input = vec![b'a'; MAX_TERMINAL_COLUMNS * 16];

    stream.consume(&input);

    assert_eq!(stream.parser.screen().size(), (1, TERMINAL_BACKING_COLUMNS));
    assert_eq!(stream.visible_line(), vec![b'a'; MAX_TERMINAL_COLUMNS]);
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);
}

#[test]
fn terminal_stream_handles_wide_characters_at_and_after_the_boundary() {
    let mut stream = DecodedStream::new();
    stream.consume(&vec![b'a'; MAX_TERMINAL_COLUMNS - 2]);

    stream.consume("界界".as_bytes());

    let mut expected = vec![b'a'; MAX_TERMINAL_COLUMNS - 2];
    expected.extend_from_slice("界".as_bytes());
    assert_eq!(stream.visible_line(), expected);
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);
}

#[test]
fn terminal_stream_keeps_combining_character_on_the_boundary_cell() {
    let mut stream = DecodedStream::new();
    stream.consume(&vec![b'a'; MAX_TERMINAL_COLUMNS - 1]);

    stream.consume("e\u{301}".as_bytes());

    let mut expected = vec![b'a'; MAX_TERMINAL_COLUMNS - 1];
    expected.extend_from_slice("e\u{301}".as_bytes());
    assert_eq!(stream.visible_line(), expected);
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);
}

#[test]
fn terminal_stream_caps_cursor_movement_to_the_visible_width() {
    let mut stream = DecodedStream::new();

    stream.consume(b"abc\x1b[9999C");
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS);

    stream.consume(b"\x1b[2DX");
    assert_eq!(stream.cursor_column(), MAX_TERMINAL_COLUMNS - 1);
    assert_eq!(stream.cursor_back_from_end(), 0);
}

#[test]
fn styled_completions_keep_shell_colors_and_reset_afterwards() {
    let mut stream = DecodedStream::new();

    let completed = stream.consume_styled(b"\x1b[32mgreen\nnext");

    assert_eq!(
        completed,
        [(b"green\n".to_vec(), b"\x1b[32mgreen\x1b[0m".to_vec())]
    );
    assert_eq!(stream.visible_line(), b"next");
}
