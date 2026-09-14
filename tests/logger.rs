use super::*;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn test_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "brtt-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn channel_paths_insert_suffix_before_extension() {
    assert_eq!(
        channel_path(Path::new("capture.log"), 2),
        PathBuf::from("capture.ch2.log")
    );
    assert_eq!(
        channel_path(Path::new("capture"), 2),
        PathBuf::from("capture.ch2")
    );
    assert_eq!(
        channel_path(Path::new("logs/capture"), 2),
        PathBuf::from("logs/capture.ch2")
    );
}

#[cfg(unix)]
#[test]
fn channel_paths_preserve_non_utf8_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let path = Path::new(OsStr::from_bytes(b"capture-\xff.log"));
    let channel_path = channel_path(path, 2);

    assert_eq!(channel_path.as_os_str().as_bytes(), b"capture-\xff.ch2.log");
}

#[test]
fn merged_decoded_logs_are_channel_tagged() {
    let path = test_path("merged");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, true)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(0, b"one\n").unwrap();
    logger.write_defmt_decoded(1, b"two\n").unwrap();
    logger.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"[ch0] one\n[ch1] two\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn raw_merged_logs_reject_multiple_channels() {
    let path = test_path("raw");
    assert!(Logger::new(Some(&path), false, LogFormat::Raw, true).is_err());
}

#[test]
fn merged_decoded_logs_keep_partial_channels_separate() {
    let path = test_path("merged-partial");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, true)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(0, b"foo").unwrap();
    logger.write_defmt_decoded(1, b"bar\n").unwrap();
    logger.write_defmt_decoded(0, b"\n").unwrap();
    logger.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"[ch1] bar\n[ch0] foo\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn per_channel_raw_logs_preserve_bytes() {
    let path = test_path("raw-per-channel.log");
    let mut logger = Logger::new(Some(&path), true, LogFormat::Raw, false)
        .unwrap()
        .unwrap();
    logger.write_bytes(1, &[0, 1, 0xff]).unwrap();
    logger.flush().unwrap();
    let channel_path = channel_path(&path, 1);
    assert_eq!(fs::read(&channel_path).unwrap(), &[0, 1, 0xff]);
    fs::remove_file(channel_path).unwrap();
}

#[test]
fn per_channel_decoded_logs_do_not_need_channel_tags() {
    let path = test_path("decoded-per-channel.log");
    let mut logger = Logger::new(Some(&path), true, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(1, b"message\n").unwrap();
    logger.flush().unwrap();

    let channel_path = channel_path(&path, 1);
    assert_eq!(fs::read(&channel_path).unwrap(), b"message\n");
    fs::remove_file(channel_path).unwrap();
}

#[test]
fn per_channel_decoded_logs_ignore_merged_channel_tag_setting() {
    let path = test_path("decoded-per-channel-multiple.log");
    let mut logger = Logger::new(Some(&path), true, LogFormat::Decoded, true)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(1, b"message\n").unwrap();
    logger.flush().unwrap();

    let channel_path = channel_path(&path, 1);
    assert_eq!(fs::read(&channel_path).unwrap(), b"message\n");
    fs::remove_file(channel_path).unwrap();
}

#[test]
fn merged_decoded_log_finalizes_partial_lines_in_channel_order() {
    let path = test_path("decoded-ordered-tails.log");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, true)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(2, b"two").unwrap();
    logger.write_defmt_decoded(0, b"zero").unwrap();
    logger.reset().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"[ch0] zero\n[ch2] two\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_flushes_an_unfinished_line() {
    let path = test_path("decoded-tail");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(0, b"unfinished").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"unfinished");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_collapses_terminal_redraws() {
    let path = test_path("decoded-redraw");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger
        .write_chars(0, b"\r\x1b[2K> help\r\x1b[2K> \r\x1b[2K> help\r\n")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"> help\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_strips_sgr_from_terminal_output() {
    let path = test_path("decoded-sgr");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger
        .write_chars(0, b"\x1b[32mgreen \x1b[31mred\x1b[0m\n")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"green red\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_handles_escape_sequences_split_between_reads() {
    let path = test_path("decoded-split-escape");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"old\r\x1b[").unwrap();
    logger.write_chars(0, b"2Knew\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"new\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_keeps_echoed_commands() {
    let path = test_path("decoded-command");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"> pwd\r\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"> pwd\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_overwrites_without_truncating_the_tail() {
    let path = test_path("decoded-overwrite");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"abc\x1b[1GX\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"Xbc\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn decoded_logger_keeps_tail_when_tab_moves_cursor_backwards() {
    let path = test_path("decoded-tab");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"abcdefghij\x1b[3G\t\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"abcdefghij\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn plain_decoded_text_does_not_interpret_terminal_controls() {
    let path = test_path("decoded-plain-text");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_defmt_decoded(0, b"value: \x1b[2K\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"value: \x1b[2K\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_logger_preserves_utf8_and_display_width() {
    let path = test_path("terminal-utf8");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, "ż界\n".as_bytes()).unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), "ż界\n".as_bytes());
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_logger_keeps_combining_marks_with_their_base_character() {
    let path = test_path("terminal-combining");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, "e\u{301}\n".as_bytes()).unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), "e\u{301}\n".as_bytes());
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_logger_erases_the_cursor_cell_with_csi_one_k() {
    let path = test_path("terminal-erase-before");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"abc\x1b[1K\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_logger_handles_zephyr_erase_display_after_backspace() {
    let path = test_path("terminal-zephyr-backspace");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger
        .write_chars(0, b"rtt:~$ abc\x1b[1D\x1b[J\r\n")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"rtt:~$ ab\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_logger_deletes_characters_with_csi_p() {
    let path = test_path("terminal-delete-char");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"abc\x1b[D\x1b[P\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"ab\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_logger_flush_preserves_parser_state() {
    let path = test_path("terminal-flush-state");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"old\r\x1b[").unwrap();
    logger.flush().unwrap();
    logger.write_chars(0, b"2Knew\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"oldnew\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn logger_reset_clears_partial_terminal_state() {
    let path = test_path("reset-state");
    let mut logger = Logger::new(Some(&path), false, LogFormat::Decoded, false)
        .unwrap()
        .unwrap();
    logger.write_chars(0, b"boot").unwrap();
    logger.reset().unwrap();
    logger.write_chars(0, b"next\n").unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"boot\nnext\n");
    fs::remove_file(path).unwrap();
}
