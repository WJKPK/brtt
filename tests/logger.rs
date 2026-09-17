use super::*;
use crate::channel::ChannelId;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn source(channel: usize) -> CoreChannel {
    CoreChannel {
        core: CoreId::new(0),
        channel: ChannelId::from_cli(u32::try_from(channel).unwrap(), "test").unwrap(),
    }
}

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
        channel_path(Path::new("capture.log"), source(2)),
        PathBuf::from("capture.c0.ch2.log")
    );
    assert_eq!(
        channel_path(Path::new("capture"), source(2)),
        PathBuf::from("capture.c0.ch2")
    );
    assert_eq!(
        channel_path(Path::new("logs/capture"), source(2)),
        PathBuf::from("logs/capture.c0.ch2")
    );
}

#[cfg(unix)]
#[test]
fn channel_paths_preserve_non_utf8_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let path = Path::new(OsStr::from_bytes(b"capture-\xff.log"));
    let channel_path = channel_path(path, source(2));

    assert_eq!(
        channel_path.as_os_str().as_bytes(),
        b"capture-\xff.c0.ch2.log"
    );
}

#[test]
fn merged_decoded_logs_are_channel_tagged() {
    let path = test_path("merged");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    logger.write_defmt_decoded(source(0), b"one\n").unwrap();
    logger.write_defmt_decoded(source(1), b"two\n").unwrap();
    logger.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"[ch0] one\n[ch1] two\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn raw_merged_logs_reject_multiple_channels() {
    let path = test_path("raw");
    assert!(Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Raw,
        true,
        false
    )
    .is_err());
}

#[test]
fn merged_decoded_logs_keep_partial_channels_separate() {
    let path = test_path("merged-partial");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    logger.write_defmt_decoded(source(0), b"foo").unwrap();
    logger.write_defmt_decoded(source(1), b"bar\n").unwrap();
    logger.write_defmt_decoded(source(0), b"\n").unwrap();
    logger.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"[ch1] bar\n[ch0] foo\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn per_channel_raw_logs_preserve_bytes() {
    let path = test_path("raw-per-channel.log");
    let mut logger = Logger::new(
        Some(&LogDestination::PerChannel(path.clone())),
        LogFormat::Raw,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger.write_bytes(source(1), &[0, 1, 0xff]).unwrap();
    logger.flush().unwrap();
    let channel_path = channel_path(&path, source(1));
    assert_eq!(fs::read(&channel_path).unwrap(), &[0, 1, 0xff]);
    fs::remove_file(channel_path).unwrap();
}

#[test]
fn per_channel_decoded_logs_ignore_merged_channel_tag_setting() {
    let path = test_path("decoded-per-channel-multiple.log");
    let mut logger = Logger::new(
        Some(&LogDestination::PerChannel(path.clone())),
        LogFormat::Decoded,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    logger.write_defmt_decoded(source(1), b"message\n").unwrap();
    logger.flush().unwrap();

    let channel_path = channel_path(&path, source(1));
    assert_eq!(fs::read(&channel_path).unwrap(), b"message\n");
    fs::remove_file(channel_path).unwrap();
}

#[test]
fn decoded_logger_flushes_an_unfinished_line() {
    let path = test_path("decoded-tail");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_defmt_decoded(source(0), b"unfinished")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"unfinished");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_decoded_partial_is_buffered_until_flush() {
    let path = test_path("terminal-partial");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(source(0), &[], b"boot")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"boot");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_decoded_partial_is_replaced_by_later_decode() {
    let path = test_path("terminal-replace-partial");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(source(0), &[], b"par")
        .unwrap();
    logger
        .write_terminal_decoded(source(0), &[b"partial line\n".to_vec()], b"")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"partial line\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_decoded_lines_include_channel_tags_when_merged() {
    let path = test_path("terminal-channel-tags");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded(source(0), &[b"zero\n".to_vec()], b"")
        .unwrap();
    logger
        .write_terminal_decoded(source(1), &[b"one\n".to_vec()], b"")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"[ch0] zero\n[ch1] one\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn plain_decoded_text_does_not_interpret_terminal_controls() {
    let path = test_path("decoded-plain-text");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_defmt_decoded(source(0), b"value: \x1b[2K\n")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"value: \x1b[2K\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_decoded_flush_preserves_cached_partial() {
    let path = test_path("terminal-flush-cache");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(source(0), &[], b"old")
        .unwrap();
    logger.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"old");

    // Flush leaves the cached partial in place; the next decode overwrites it
    // while its completed line is appended after the already-flushed tail.
    logger
        .write_terminal_decoded(source(0), &[b"new\n".to_vec()], b"")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"oldnew\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_decoded_tails_are_ordered_with_defmt_tails() {
    let path = test_path("terminal-mixed-tails");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        true,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(source(2), &[], b"two")
        .unwrap();
    logger.write_defmt_decoded(source(0), b"zero").unwrap();
    logger.reset().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"[ch0] zero\n[ch2] two\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_decoded_writes_are_ignored_in_raw_mode() {
    let path = test_path("terminal-raw-ignored.log");
    let mut logger = Logger::new(
        Some(&LogDestination::PerChannel(path.clone())),
        LogFormat::Raw,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded(source(0), &[b"decoded\n".to_vec()], b"partial")
        .unwrap();
    logger.flush().unwrap();

    let channel_path = channel_path(&path, source(0));
    // No terminal channel file is created in raw mode; only raw bytes create files.
    assert!(!channel_path.exists());
    if channel_path.exists() {
        fs::remove_file(channel_path).unwrap();
    }
}

#[test]
fn ingest_fragment_assembles_line_split_across_calls() {
    let mut assembly = LineAssembly::buffered();
    assert!(assembly.ingest_fragment(b"hel").is_empty());
    assert!(assembly.ingest_fragment(b"lo").is_empty());
    assert_eq!(
        assembly.ingest_fragment(b"\nrest"),
        vec![b"hello\n".to_vec()]
    );
    assert_eq!(assembly.partial_line(), b"rest");
    assert_eq!(assembly.ingest_fragment(b"\n"), vec![b"rest\n".to_vec()]);
    assert!(assembly.partial_line().is_empty());
}

#[test]
fn merged_logs_name_the_core_when_cores_shown() {
    let path = test_path("merged-cores");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        true,
        true,
    )
    .unwrap()
    .unwrap();
    logger.write_defmt_decoded(source(0), b"zero\n").unwrap();
    logger
        .write_defmt_decoded(
            CoreChannel {
                core: CoreId::new(1),
                channel: ChannelId::from_cli(0, "test").unwrap(),
            },
            b"one\n",
        )
        .unwrap();
    logger.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"[c0:ch0] zero\n[c1:ch0] one\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn logger_reset_core_preserves_other_cores() {
    let path = test_path("reset-one-core");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        true,
    )
    .unwrap()
    .unwrap();
    let other = CoreChannel {
        core: CoreId::new(1),
        channel: ChannelId::from_cli(0, "test").unwrap(),
    };
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(source(0), &[], b"core0-partial")
        .unwrap();
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(other, &[], b"core1-partial")
        .unwrap();

    logger.reset_core(CoreId::new(1)).unwrap();
    logger.flush().unwrap();

    // Core 1 finalized with a newline; core 0 stayed buffered until flush.
    assert_eq!(fs::read(&path).unwrap(), b"core1-partial\ncore0-partial");
    fs::remove_file(path).unwrap();
}

#[test]
fn logger_reset_clears_partial_terminal_state() {
    let path = test_path("reset-state");
    let mut logger = Logger::new(
        Some(&LogDestination::Merged(path.clone())),
        LogFormat::Decoded,
        false,
        false,
    )
    .unwrap()
    .unwrap();
    logger
        .write_terminal_decoded::<_, &Vec<u8>>(source(0), &[], b"boot")
        .unwrap();
    logger.reset().unwrap();
    logger
        .write_terminal_decoded(source(0), &[b"next\n".to_vec()], b"")
        .unwrap();
    logger.flush().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"boot\nnext\n");
    fs::remove_file(path).unwrap();
}
