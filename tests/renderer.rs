use super::*;
use crate::cli::{ChannelEncoding, ChannelSpec};
use crate::terminal::{DecodedStream, PartialView};
use brtt::rtt::{RttDiscovery, ScanRegion};
use std::collections::HashMap;
use std::time::Duration;

struct ChunkFeed {
    decoders: HashMap<ChannelId, DecodedStream>,
}

impl ChunkFeed {
    fn new() -> Self {
        Self {
            decoders: HashMap::new(),
        }
    }

    fn chunk(&mut self, state: &SessionState, channel: ChannelId, bytes: &[u8]) -> TerminalChunk {
        let styled = state.is_interactive();
        self.decoders
            .entry(channel)
            .or_insert_with(DecodedStream::new)
            .consume_chunk(bytes, styled)
    }
}

fn render_bytes_chunked(
    state: &mut SessionState,
    feed: &mut ChunkFeed,
    channel: ChannelId,
    bytes: &[u8],
    timestamp: Instant,
    output: &mut Vec<u8>,
) {
    let chunk = feed.chunk(state, channel, bytes);
    render_terminal_chunk(channel, &chunk, timestamp, state, output).unwrap();
}

#[test]
fn help_lists_current_commands() {
    let mut output = Vec::new();

    write_help(&mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("q  Quit"));
    assert!(output.contains("?  Show this help"));
    assert!(output.contains("c  Show configuration"));
    assert!(output.contains("l  Clear screen"));
    assert!(output.contains("t  Toggle timestamps"));
    assert!(output.contains("e  Toggle local echo"));
    assert!(output.contains("R  Reset target"));
    assert!(output.contains("Ctrl-C is sent to the target"));
}

#[test]
fn session_banner_shows_escape_help() {
    let mut output = Vec::new();

    write_session_banner(&mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("Press ctrl-t ? for help"));
    assert!(output.contains("Connected to target"));
}

#[test]
fn timestamps_are_added_once_per_line_across_partial_events() {
    let mut state = SessionState::new();
    state.timestamps = true;
    let timestamp = state.started + Duration::from_millis(123);
    let mut output = Vec::new();

    render_bytes(b"partial", timestamp, &mut state, &mut output).unwrap();
    render_bytes(b" line\nnext", timestamp, &mut state, &mut output).unwrap();

    let expected_timestamp = (state.started_wall + chrono::Duration::milliseconds(123))
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string();
    let expected = format!("[{expected_timestamp}] partial line\r\n[{expected_timestamp}] next");
    assert_eq!(output, expected.as_bytes());
}

#[test]
fn redirected_terminal_output_buffers_fragments_until_a_complete_line() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"partial ",
        timestamp,
        &mut output,
    );
    assert!(output.is_empty());

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"line\n",
        timestamp,
        &mut output,
    );

    assert_eq!(output, b"partial line\n");
}

#[test]
fn redirected_terminal_output_finalizes_partial_lines_in_channel_order() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let timestamp = Instant::now();
    let mut renderer = Renderer::new(Vec::new(), None, None, state);
    let mut decoders: HashMap<ChannelId, DecodedStream> = HashMap::new();

    for (channel, bytes) in [(2, b"two".as_slice()), (0, b"zero".as_slice())] {
        let channel = ChannelId::new(channel);
        let styled = renderer.is_interactive();
        let chunk = decoders
            .entry(channel)
            .or_insert_with(DecodedStream::new)
            .consume_chunk(bytes, styled);
        renderer
            .render_terminal_event(channel, &chunk, timestamp)
            .unwrap();
    }
    assert!(renderer.output.is_empty());

    renderer.finish_target_epoch().unwrap();

    assert_eq!(renderer.output, b"zero\ntwo\n");
}

#[test]
fn bare_carriage_return_overwrites_from_column_zero() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"abcdef\rxy\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output, b"xycdef\n");
}

#[test]
fn toggle_status_returns_cursor_to_column_zero() {
    let mut output = Vec::new();

    write_toggle_status(&mut output, "Timestamps", true).unwrap();

    assert_eq!(output, b"\r\nTimestamps: on\r\n");
}

#[test]
fn carriage_return_newline_is_not_rendered_as_two_lines() {
    let mut state = SessionState::new();
    state.channel_labels = true;
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"first\r\nsecond\r\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output, b"[ch0] first\r\n[ch0] second\r\n");
}

#[test]
fn completed_line_does_not_restore_its_consumed_partial_prompt() {
    let mut state = SessionState::new();
    state.channel_labels = true;
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"> ",
        Instant::now(),
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"help\r\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output, b"[ch0] > \r\x1b[2K[ch0] > help\r\n");
}

#[test]
fn zephyr_backspace_erases_the_deleted_character() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"rtt:~$ abc",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[1D\x1b[J",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\r\n",
        timestamp,
        &mut output,
    );

    assert_eq!(
        output,
        b"rtt:~$ abc\r\x1b[2K\x1b7rtt:~$ ab\x1b8\x1b[9C\r\x1b[2Krtt:~$ ab\r\n"
    );
}

#[test]
fn embassy_backspace_erases_the_deleted_character() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"> abc",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[D\x1b[P",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\r\n",
        timestamp,
        &mut output,
    );

    assert_eq!(
        output,
        b"> abc\r\x1b[2K\x1b7> ab\x1b8\x1b[4C\r\x1b[2K> ab\r\n"
    );
}

#[test]
fn styled_prompt_retains_color_after_cursor_delete() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[32m> abc",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[D\x1b[P",
        timestamp,
        &mut output,
    );

    assert_eq!(
        output,
        b"\x1b[32m> abc\r\x1b[2K\x1b7\x1b[32m> ab\x1b8\x1b[4C\x1b[m\x1b[32m"
    );
}

#[test]
fn terminal_redraw_preserves_mixed_color_spans() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[31mred\x1b[34mblue\x1b[D",
        Instant::now(),
        &mut output,
    );

    assert_eq!(
        output,
        b"\x1b7\x1b[31mred\x1b[34mblue\x1b8\x1b[6C\x1b[m\x1b[34m"
    );
}

#[test]
fn erasing_the_entire_partial_line_clears_the_foreground() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"> abc",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\r\x1b[2K",
        timestamp,
        &mut output,
    );

    assert_eq!(output, b"> abc\r\x1b[2K");
    assert!(state.foreground().is_none());
}

#[test]
fn terminal_redraw_restores_the_modeled_cursor_position() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"> abc",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[2D",
        timestamp,
        &mut output,
    );

    assert_eq!(output, b"> abc\r\x1b[2K\x1b7> abc\x1b8\x1b[3C");
}

#[test]
fn zephyr_help_output_is_followed_by_the_partial_prompt() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"rtt:~$ help",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\r\nShell commands\r\nhelp  Show help\r\nrtt:~$ ",
        timestamp,
        &mut output,
    );

    assert!(output.ends_with(b"rtt:~$ "));
    assert_eq!(state.foreground().unwrap().bytes, b"rtt:~$ ");
}

#[test]
fn config_and_clear_screen_outputs_include_session_settings() {
    let config = SessionConfig {
        probe: "probe-id".to_string(),
        chip: "nRF52840_xxAA".to_string(),
        up_specs: vec![ChannelSpec {
            index: 2,
            mode: ChannelEncoding::Terminal,
        }],
        down_channel: Some(ChannelId::new(1)),
        poll_interval: Duration::from_millis(10),
        reset: false,
        timestamps: false,
        defmt: None,
        defmt_filters: None,
        color: crate::cli::ColorMode::Never,
        log: None,
        discovery: RttDiscovery::Fixed(ScanRegion::Exact(0x2000_0000)),
    };
    let state = SessionState::new();
    let mut output = Vec::new();

    write_config(&config, &state, &mut output).unwrap();
    let config_output = String::from_utf8(output).unwrap();
    assert!(config_output.contains("Probe: probe-id"));
    assert!(config_output.contains("Chip: nRF52840_xxAA"));
    assert!(config_output.contains("Up channels: 2:terminal"));
    assert!(config_output.contains("Down channel: 1"));
    assert!(config_output.contains("Poll interval: 10 ms"));

    let mut clear_output = Vec::new();
    clear_screen(&mut clear_output).unwrap();
    assert!(clear_output.starts_with(b"\x1b[2J"));
}

#[test]
fn terminal_chunks_are_rendered_with_channel_labels_in_read_order() {
    let mut output = Vec::new();
    let mut state = SessionState::new();
    state.channel_labels = true;
    let mut feed = ChunkFeed::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(2),
        b"log\n",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"shell",
        timestamp,
        &mut output,
    );

    assert_eq!(output, b"[ch2] log\r\n[ch0] shell");
}

#[test]
fn multiple_channels_are_labeled_on_each_line() {
    let mut output = Vec::new();
    let mut state = SessionState::new();
    state.channel_labels = true;
    let mut feed = ChunkFeed::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"zero\none",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(1),
        b"one\n",
        timestamp,
        &mut output,
    );

    assert_eq!(
        output,
        b"[ch0] zero\r\n[ch0] one\r\x1b[2K[ch1] one\r\n[ch0] one"
    );
}

#[test]
fn channel_labels_use_stable_palette_colors() {
    assert_eq!(channel_color(ChannelId::new(0)), "\x1b[36m");
    assert_eq!(
        channel_color(ChannelId::new(6)),
        channel_color(ChannelId::new(0))
    );

    let mut output = Vec::new();
    let mut state = SessionState::new();
    state.channel_labels = true;
    state.color = true;
    let mut feed = ChunkFeed::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(1),
        b"line\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output, b"\x1b[35m[ch1] \x1b[0mline\r\n");
}

#[test]
fn defmt_level_color_composes_after_channel_color() {
    let frame = DecodedFrame {
        message: "bad".into(),
        timestamp: None,
        level: Some(defmt_parser::Level::Error),
        module: None,
    };
    let mut output = Vec::new();
    let mut state = SessionState::new();
    state.channel_labels = true;
    state.color = true;

    render_defmt_frame(
        ChannelId::new(1),
        &frame,
        Instant::now(),
        None,
        &mut state,
        None,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"\x1b[35m[ch1] \x1b[0m\x1b[31merror bad\r\n\x1b[0m");
}

#[test]
fn filtered_defmt_frames_are_not_rendered_or_logged() {
    let frame = DecodedFrame {
        message: "quiet".into(),
        timestamp: None,
        level: Some(defmt_parser::Level::Info),
        module: Some("app"),
    };
    let filters = vec![Filter {
        module: "".into(),
        level: defmt_parser::Level::Warn,
    }];
    let mut output = Vec::new();
    let mut state = SessionState::new();

    render_defmt_frame(
        ChannelId::new(0),
        &frame,
        Instant::now(),
        Some(&filters),
        &mut state,
        None,
        &mut output,
    )
    .unwrap();

    assert!(output.is_empty());
    assert_eq!(state.defmt_decode_warnings, 0);
}

#[test]
fn reset_target_clears_renderer_state() {
    let mut state = SessionState::new();
    state.line_start = false;
    state.last_channel = Some(ChannelId::new(1));
    state.set_foreground(ForegroundLine {
        channel: ChannelId::new(1),
        bytes: b"> ".to_vec(),
    });
    state.partials.insert(
        ChannelId::new(1),
        PartialView {
            log: b"> ".to_vec(),
            display: b"> ".to_vec(),
            overlay: b"> ".to_vec(),
        },
    );

    state.reset_target();

    assert!(state.line_start);
    assert_eq!(state.last_channel, None);
    assert!(state.foreground().is_none());
    assert!(state.partials.is_empty());
}

#[test]
fn terminal_lines_are_bounded_instead_of_growing_without_bound() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let long = vec![b'a'; 16 * 1024 + 64];

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        &long,
        Instant::now(),
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output.len(), 4097);
}

#[test]
fn terminal_output_preserves_sgr_colors_without_cursor_rewrites() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[31mred\x1b[0m\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output, b"\x1b[31mred\x1b[0m\r\n");
}

#[test]
fn raw_classifier_handles_escape_sequences_split_across_chunks() {
    let mut stream = DecodedStream::new();

    let first = stream.consume_chunk(b"\x1b[3", false);
    assert!(first.lines.is_empty());
    assert_eq!(first.partial.display, b"\x1b[3");

    let second = stream.consume_chunk(b"1mred\x1b[", false);
    assert!(second.lines.is_empty());
    assert_eq!(second.partial.display, b"\x1b[31mred\x1b[");

    let third = stream.consume_chunk(b"0m\nabc\x1b[2", false);
    assert_eq!(
        third
            .lines
            .iter()
            .map(|line| line.display.clone())
            .collect::<Vec<_>>(),
        [b"\x1b[31mred\x1b[0m".to_vec()]
    );
    assert_eq!(third.partial.display, b"abc\x1b[2");

    let fourth = stream.consume_chunk(b"D\x1b[J\n", false);
    assert_eq!(
        fourth
            .lines
            .iter()
            .map(|line| line.display.clone())
            .collect::<Vec<_>>(),
        [b"a".to_vec()]
    );
    assert!(fourth.partial.display.is_empty());
}

#[test]
fn overlong_unterminated_escape_is_bounded_and_uses_terminal_rendering() {
    use crate::terminal::MAX_RAW_ESCAPE_BYTES;

    let mut stream = DecodedStream::new();
    let mut input = b"prefix\x1b[".to_vec();
    input.extend(std::iter::repeat_n(b'1', MAX_RAW_ESCAPE_BYTES * 4));

    let chunk = stream.consume_chunk(&input, false);

    assert!(chunk.lines.is_empty());
    // An overlong escape forces terminal rendering: the presentation falls
    // back to the VT-decoded line instead of echoing raw bytes.
    assert_eq!(chunk.partial.display, chunk.partial.log);
}

#[test]
fn backspaced_line_keeps_shell_colors_after_enter() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[32mrtt:~$ abc",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\x1b[1D\x1b[J",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::new(0),
        b"\r\n",
        timestamp,
        &mut output,
    );

    // The `\x1b[K` sequences come from vt100's row formatter clearing the
    // erased (but still green-attributed) cell; they are visual no-ops here.
    assert_eq!(
        output,
        b"\x1b[32mrtt:~$ abc\r\x1b[2K\x1b7\x1b[32mrtt:~$ ab\x1b[K\x1b8\x1b[9C\x1b[m\x1b[32m\r\x1b[2K\x1b[32mrtt:~$ ab\x1b[K\x1b[0m\r\n"
    );
}

fn renderer_log_path(name: &str) -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};

    std::env::temp_dir().join(format!(
        "brtt-renderer-{name}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn terminal_event_split_escape_shares_single_decode_between_display_and_log() {
    use std::fs;

    let path = renderer_log_path("shared-split");
    let logger = Logger::new(
        Some(&crate::cli::LogDestination::Merged(path.clone())),
        crate::cli::LogFormat::Decoded,
        false,
    )
    .unwrap()
    .unwrap();
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut renderer = Renderer::new(Vec::new(), Some(logger), None, state);
    let mut decoder = DecodedStream::new();
    let timestamp = Instant::now();

    for bytes in [b"old\r\x1b[".as_slice(), b"2Knew\n".as_slice()] {
        let styled = renderer.is_interactive();
        let chunk = decoder.consume_chunk(bytes, styled);
        renderer
            .render_terminal_event(ChannelId::new(0), &chunk, timestamp)
            .unwrap();
    }
    renderer.finish_session().unwrap();

    assert_eq!(renderer.output, b"new\n");
    assert_eq!(fs::read(&path).unwrap(), b"new\n");
    fs::remove_file(path).unwrap();
}

#[test]
fn terminal_event_strips_sgr_for_log_but_preserves_it_for_display() {
    use std::fs;

    let path = renderer_log_path("shared-sgr");
    let logger = Logger::new(
        Some(&crate::cli::LogDestination::Merged(path.clone())),
        crate::cli::LogFormat::Decoded,
        false,
    )
    .unwrap()
    .unwrap();
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut renderer = Renderer::new(Vec::new(), Some(logger), None, state);
    let mut decoder = DecodedStream::new();

    let styled = renderer.is_interactive();
    let chunk = decoder.consume_chunk(b"\x1b[32mgreen \x1b[31mred\x1b[0m\n", styled);
    assert_eq!(
        chunk
            .lines
            .iter()
            .map(|line| line.log.clone())
            .collect::<Vec<_>>(),
        [b"green red\n".to_vec()]
    );
    renderer
        .render_terminal_event(ChannelId::new(0), &chunk, Instant::now())
        .unwrap();
    renderer.finish_session().unwrap();

    assert_eq!(renderer.output, b"\x1b[32mgreen \x1b[31mred\x1b[0m\n");
    assert_eq!(fs::read(&path).unwrap(), b"green red\n");
    fs::remove_file(path).unwrap();
}
