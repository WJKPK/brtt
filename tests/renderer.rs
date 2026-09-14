use super::*;
use crate::cli::{ChannelEncoding, ChannelSpec};
use brtt::rtt::{RttDiscovery, ScanRegion};
use std::time::Duration;

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
fn timestamps_are_disabled_by_default() {
    let mut state = SessionState::new();
    let mut output = Vec::new();

    render_bytes(b"text\n", Instant::now(), &mut state, &mut output).unwrap();

    assert_eq!(output, b"text\r\n");
}

#[test]
fn redirected_terminal_output_buffers_fragments_until_a_complete_line() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"partial ",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    assert!(output.is_empty());

    render_terminal_chunk(
        ChannelId::new(0),
        b"line\n",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"partial line\n");
}

#[test]
fn redirected_terminal_output_finalizes_partial_lines_in_channel_order() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let timestamp = Instant::now();
    let mut renderer = Renderer::new(Vec::new(), None, None, state);

    renderer
        .render_terminal_event(ChannelId::new(2), b"two", timestamp)
        .unwrap();
    renderer
        .render_terminal_event(ChannelId::new(0), b"zero", timestamp)
        .unwrap();
    assert!(renderer.output.is_empty());

    renderer.finish_target_epoch().unwrap();

    assert_eq!(renderer.output, b"zero\ntwo\n");
}

#[test]
fn bare_carriage_return_overwrites_from_column_zero() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut output = Vec::new();

    render_terminal_chunk(
        ChannelId::new(0),
        b"abcdef\rxy\n",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

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
    let mut output = Vec::new();

    render_terminal_chunk(
        ChannelId::new(0),
        b"first\r\nsecond\r\n",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"[ch0] first\r\n[ch0] second\r\n");
}

#[test]
fn completed_line_does_not_restore_its_consumed_partial_prompt() {
    let mut state = SessionState::new();
    state.channel_labels = true;
    let mut output = Vec::new();

    render_terminal_chunk(
        ChannelId::new(0),
        b"> ",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"help\r\n",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"[ch0] > \r\x1b[2K[ch0] > help\r\n");
}

#[test]
fn zephyr_backspace_erases_the_deleted_character() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"rtt:~$ abc",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[1D\x1b[J",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\r\n",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(
        output,
        b"rtt:~$ abc\r\x1b[2K\x1b7rtt:~$ ab\x1b8\x1b[9C\r\x1b[2Krtt:~$ ab\r\n"
    );
}

#[test]
fn embassy_backspace_erases_the_deleted_character() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"> abc",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[D\x1b[P",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\r\n",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(
        output,
        b"> abc\r\x1b[2K\x1b7> ab\x1b8\x1b[4C\r\x1b[2K> ab\r\n"
    );
}

#[test]
fn styled_prompt_retains_color_after_cursor_delete() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[32m> abc",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[D\x1b[P",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(
        output,
        b"\x1b[32m> abc\r\x1b[2K\x1b7\x1b[32m> ab\x1b8\x1b[4C\x1b[m\x1b[32m"
    );
}

#[test]
fn terminal_redraw_preserves_mixed_color_spans() {
    let mut state = SessionState::new();
    let mut output = Vec::new();

    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[31mred\x1b[34mblue\x1b[D",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(
        output,
        b"\x1b7\x1b[31mred\x1b[34mblue\x1b8\x1b[6C\x1b[m\x1b[34m"
    );
}

#[test]
fn erasing_the_entire_partial_line_clears_the_foreground() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"> abc",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\r\x1b[2K",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"> abc\r\x1b[2K");
    assert!(state.foreground().is_none());
}

#[test]
fn terminal_redraw_restores_the_modeled_cursor_position() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"> abc",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[2D",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"> abc\r\x1b[2K\x1b7> abc\x1b8\x1b[3C");
}

#[test]
fn zephyr_help_output_is_followed_by_the_partial_prompt() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"rtt:~$ help",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\r\nShell commands\r\nhelp  Show help\r\nrtt:~$ ",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

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
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(2),
        b"log\n",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"shell",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"[ch2] log\r\n[ch0] shell");
}

#[test]
fn multiple_channels_are_labeled_on_each_line() {
    let mut output = Vec::new();
    let mut state = SessionState::new();
    state.channel_labels = true;
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"zero\none",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(1),
        b"one\n",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

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

    render_terminal_chunk(
        ChannelId::new(1),
        b"line\n",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

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
    state
        .streams
        .insert(ChannelId::new(1), SessionStream::new());

    state.reset_target();

    assert!(state.line_start);
    assert_eq!(state.last_channel, None);
    assert!(state.foreground().is_none());
    assert!(state.streams.is_empty());
}

#[test]
fn terminal_lines_are_bounded_instead_of_growing_without_bound() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut output = Vec::new();
    let long = vec![b'a'; 16 * 1024 + 64];

    render_terminal_chunk(
        ChannelId::new(0),
        &long,
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\n",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output.len(), 4097);
}

#[test]
fn terminal_output_preserves_sgr_colors_without_cursor_rewrites() {
    let mut state = SessionState::new();
    let mut output = Vec::new();

    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[31mred\x1b[0m\n",
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"\x1b[31mred\x1b[0m\r\n");
}

#[test]
fn raw_parser_handles_escape_sequences_split_across_chunks() {
    let mut stream = SessionStream::new();

    let first = stream.consume(b"\x1b[3", false);
    assert!(first.complete.is_empty());
    assert_eq!(first.partial, b"\x1b[3");

    let second = stream.consume(b"1mred\x1b[", false);
    assert!(second.complete.is_empty());
    assert_eq!(second.partial, b"\x1b[31mred\x1b[");

    let third = stream.consume(b"0m\nabc\x1b[2", false);
    assert_eq!(third.complete, [b"\x1b[31mred\x1b[0m".to_vec()]);
    assert_eq!(third.partial, b"abc\x1b[2");

    let fourth = stream.consume(b"D\x1b[J\n", false);
    assert_eq!(fourth.complete, [b"a".to_vec()]);
    assert!(fourth.partial.is_empty());
}

#[test]
fn overlong_unterminated_escape_is_bounded_and_uses_terminal_rendering() {
    let mut stream = SessionStream::new();
    let mut input = b"prefix\x1b[".to_vec();
    input.extend(std::iter::repeat_n(b'1', MAX_RAW_ESCAPE_BYTES * 4));

    let output = stream.consume(&input, false);

    assert!(output.complete.is_empty());
    assert!(stream.requires_terminal_rendering);
    match &stream.raw_state {
        RawInputState::Escape(bytes) => assert_eq!(bytes.len(), MAX_RAW_ESCAPE_BYTES),
        _ => panic!("expected an in-progress escape sequence"),
    }
    assert_eq!(output.partial, stream.terminal.visible_line());
}

#[test]
fn backspaced_line_keeps_shell_colors_after_enter() {
    let mut state = SessionState::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[32mrtt:~$ abc",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\x1b[1D\x1b[J",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();
    render_terminal_chunk(
        ChannelId::new(0),
        b"\r\n",
        timestamp,
        &mut state,
        &mut output,
    )
    .unwrap();

    // The `\x1b[K` sequences come from vt100's row formatter clearing the
    // erased (but still green-attributed) cell; they are visual no-ops here.
    assert_eq!(
        output,
        b"\x1b[32mrtt:~$ abc\r\x1b[2K\x1b7\x1b[32mrtt:~$ ab\x1b[K\x1b8\x1b[9C\x1b[m\x1b[32m\r\x1b[2K\x1b[32mrtt:~$ ab\x1b[K\x1b[0m\r\n"
    );
}
