use super::*;
use crate::terminal::{DecodedStream, TerminalLine};
use crate::cli::{ChannelEncoding, ChannelSpec};
use std::time::Duration;

fn channel(index: usize) -> ChannelId {
    ChannelId::from_cli(index as u32, "test").unwrap()
}

fn source(core: usize, channel_index: usize) -> CoreChannel {
    CoreChannel {
        core: CoreId::new(core as u32),
        channel: channel(channel_index),
    }
}

fn completed(plain: &[u8], styled: &[u8]) -> TerminalChunk {
    TerminalChunk {
        lines: vec![TerminalLine {
            plain: plain.to_vec(),
            styled: styled.to_vec(),
        }],
        partial: TerminalLine {
            plain: Vec::new(),
            styled: Vec::new(),
        },
    }
}

fn partial(plain: &[u8], styled: &[u8]) -> TerminalChunk {
    TerminalChunk {
        lines: Vec::new(),
        partial: TerminalLine {
            plain: plain.to_vec(),
            styled: styled.to_vec(),
        },
    }
}

fn empty_chunk() -> TerminalChunk {
    partial(b"", b"")
}
struct ChunkFeed {
    streams: HashMap<ChannelId, DecodedStream>,
}

impl ChunkFeed {
    fn new() -> Self {
        Self { streams: HashMap::new() }
    }

    fn chunk(&mut self, state: &SessionState, channel: ChannelId, bytes: &[u8]) -> TerminalChunk {
        self.streams
            .entry(channel)
            .or_insert_with(DecodedStream::new)
            .consume_chunk(bytes, state.is_interactive())
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
    render_terminal_chunk(
        CoreChannel { core: CoreId::new(0), channel },
        chunk,
        timestamp,
        state,
        output,
    )
    .unwrap();
}

#[test]
fn timestamps_are_added_once_per_logical_line() {
    let mut state = SessionState::new();
    state.timestamps = true;

    let timestamp = state.started + Duration::from_millis(123);
    let src = source(0, 0);
    let mut output = Vec::new();

    render_channel_bytes(
        b"partial",
        src,
        timestamp,
        &mut state,
        &mut output,
        None,
    )
    .unwrap();

    render_channel_bytes(
        b" line\nnext",
        src,
        timestamp,
        &mut state,
        &mut output,
        None,
    )
    .unwrap();

    let wall = (state.started_wall + chrono::Duration::milliseconds(123))
        .format("%Y-%m-%d %H:%M:%S%.3f");

    assert_eq!(
        output,
        format!("[{wall}] partial line\r\n[{wall}] next").as_bytes()
    );
}

#[test]
fn channel_labels_are_added_at_line_starts() {
    let mut state = SessionState::new();
    state.channel_labels = true;

    let mut output = Vec::new();

    render_channel_bytes(
        b"first\nsecond\n",
        source(0, 2),
        Instant::now(),
        &mut state,
        &mut output,
        None,
    )
    .unwrap();

    assert_eq!(
        output,
        b"[ch2] first\r\n[ch2] second\r\n"
    );
}

#[test]
fn interactive_complete_line_uses_styled_representation() {
    let mut state = SessionState::new();
    let mut output = Vec::new();

    render_terminal_chunk(
        source(0, 0),
        completed(b"plain\n", b"styled"),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(
        output,
        [b"styled".as_slice(), ANSI_RESET, b"\r\n"].concat()
    );
}

#[test]
fn redirected_complete_line_uses_plain_representation() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut output = Vec::new();

    render_terminal_chunk(
        source(0, 0),
        completed(b"plain\n", b"styled-that-must-not-appear"),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"plain\n");
}

#[test]
fn redirected_partials_are_buffered_and_flushed_in_source_order() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;

    let mut renderer = Renderer::new(Vec::new(), None, state);
    let timestamp = Instant::now();

    renderer
        .render_terminal_event(
            source(0, 2),
            partial(b"two", b"ignored"),
            timestamp,
        )
        .unwrap();

    renderer
        .render_terminal_event(
            source(0, 0),
            partial(b"zero", b"ignored"),
            timestamp,
        )
        .unwrap();

    assert!(renderer.output.is_empty());

    renderer.finish_target_epoch().unwrap();

    assert_eq!(renderer.output, b"zero\ntwo\n");
}

#[test]
fn interactive_partial_becomes_foreground() {
    let mut state = SessionState::new();
    let src = source(0, 0);

    let mut output = Vec::new();

    render_terminal_chunk(
        src,
        partial(b"> ", b"> "),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"> ");

    let foreground = state.foreground().unwrap();
    assert_eq!(foreground.channel, src);
    assert_eq!(foreground.rendered, b"> ");
}

#[test]
fn background_partial_is_cached_without_touching_screen() {
    let mut state = SessionState::new();
    let foreground_source = source(0, 0);
    let background_source = source(1, 0);

    let mut output = Vec::new();

    render_terminal_chunk(
        foreground_source,
        partial(b"a> ", b"a> "),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"a> ");

    render_terminal_chunk(
        background_source,
        partial(b"b> ", b"b> "),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    // Background activity does not disturb the visible prompt.
    assert_eq!(output, b"a> ");
    assert_eq!(
        state.foreground().unwrap().channel,
        foreground_source
    );

    // But its prompt is available for a later core switch.
    let cached = state.prompts.get(&background_source.core).unwrap();
    assert_eq!(cached.channel, background_source);
    assert_eq!(cached.rendered, b"b> ");
}

#[test]
fn background_complete_line_temporarily_replaces_and_restores_foreground() {
    let mut state = SessionState::new();
    let foreground_source = source(0, 0);
    let background_source = source(1, 0);

    let mut output = Vec::new();

    render_terminal_chunk(
        foreground_source,
        partial(b"> ", b"> "),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    render_terminal_chunk(
        background_source,
        completed(b"log\n", b"log"),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    let expected = [
        b"> ".as_slice(),
        ERASE_CURRENT_LINE,
        ANSI_RESET,
        b"log",
        ANSI_RESET,
        b"\r\n",
        b"> ",
    ]
    .concat();

    assert_eq!(output, expected);

    assert_eq!(
        state.foreground().unwrap().channel,
        foreground_source
    );
}

#[test]
fn completed_line_does_not_restore_its_own_old_partial() {
    let mut state = SessionState::new();
    let src = source(0, 0);

    let mut output = Vec::new();

    render_terminal_chunk(
        src,
        partial(b"> ", b"> "),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    render_terminal_chunk(
        src,
        completed(b"> help\n", b"> help"),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    let expected = [
        b"> ".as_slice(),
        ERASE_CURRENT_LINE,
        ANSI_RESET,
        b"> help",
        ANSI_RESET,
        b"\r\n",
    ]
    .concat();

    assert_eq!(output, expected);
    assert!(state.foreground().is_none());
}

#[test]
fn empty_partial_clears_its_foreground() {
    let mut state = SessionState::new();
    let src = source(0, 0);
    let mut output = Vec::new();

    render_terminal_chunk(src, partial(b"> ", b"> "), Instant::now(), &mut state, &mut output)
        .unwrap();
    render_terminal_chunk(src, empty_chunk(), Instant::now(), &mut state, &mut output).unwrap();

    assert_eq!(output, [b"> ".as_slice(), ERASE_CURRENT_LINE, ANSI_RESET].concat());
    assert!(state.foreground().is_none());
}
#[test]
fn cached_core_prompt_can_be_restored_without_new_target_bytes() {
    let core0 = source(0, 0);
    let core1 = source(1, 0);
    let mut state = SessionState::new();
    state.channel_labels = true;
    state.show_cores = true;
    let mut feed0 = ChunkFeed::new();
    let mut feed1 = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    let chunk = feed0.chunk(&state, channel(0), b"m7:~$ ");
    render_terminal_chunk(core0, chunk, timestamp, &mut state, &mut output).unwrap();
    let chunk = feed1.chunk(&state, channel(0), b"m4:~$ ");
    render_terminal_chunk(core1, chunk, timestamp, &mut state, &mut output).unwrap();

    let mut renderer = Renderer::new(Vec::new(), None, state);
    renderer.suspend_foreground().unwrap();
    renderer.output.clear();
    assert!(renderer.show_cached_prompt(core1.core).unwrap());
    assert_eq!(renderer.output, b"[c1:ch0] m4:~$ ");
    assert_eq!(renderer.state.foreground().unwrap().channel, core1);
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
        ChannelId::from_cli(0, "test").unwrap(),
        b"rtt:~$ help",
        timestamp,
        &mut output,
    );
    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::from_cli(0, "test").unwrap(),
        b"\r\nShell commands\r\nhelp  Show help\r\nrtt:~$ ",
        timestamp,
        &mut output,
    );

    assert!(output.ends_with(b"rtt:~$ "));
    assert_eq!(state.foreground().unwrap().rendered, b"rtt:~$ ");
}

#[test]
fn config_and_clear_screen_outputs_include_session_settings() {
    let config = SessionPolicy {
        probe: "probe-id".to_string(),
        chip: "nRF52840_xxAA".to_string(),
        up_specs: vec![ChannelSpec {
            core: None,
            index: 2,
            mode: ChannelEncoding::Terminal,
        }],
        down_channel: Some(ChannelId::from_cli(1, "test").unwrap()),
        down_explicit: true,
        poll_interval: Duration::from_millis(10),
        timestamps: false,
        color: crate::cli::ColorMode::Never,
        log: None,
    };
    let mut renderer = Renderer::for_session(Vec::new(), None, &config, true, false);
    renderer.clear_screen().unwrap();
    assert_eq!(renderer.output, b"\x1b[2J\x1b[1;1H");
}

#[test]
fn erase_foreground_always_resets_terminal_attributes() {
    let src = source(0, 0);
    let mut state = SessionState::new();

    state.set_foreground(ForegroundLine {
        channel: src,
        rendered: b"\x1b[32m> ".to_vec(),
    });

    let mut output = Vec::new();

    let saved = erase_foreground(&mut state, &mut output)
        .unwrap()
        .unwrap();

    assert_eq!(saved.channel, src);
    assert_eq!(
        output,
        [ERASE_CURRENT_LINE, ANSI_RESET].concat()
    );
    assert!(state.foreground().is_none());
}

#[test]
fn reset_core_removes_only_that_cores_cached_state() {
    let core0 = source(0, 0);
    let core1 = source(1, 0);
    let mut state = SessionState::new();
    state.partials.insert(core0, b"zero".to_vec());
    state.partials.insert(core1, b"one".to_vec());
    state.prompts.insert(core0.core, ForegroundLine { channel: core0, rendered: b"zero> ".to_vec() });
    state.prompts.insert(core1.core, ForegroundLine { channel: core1, rendered: b"one> ".to_vec() });
    state.reset_core(core1.core);
    assert!(state.partials.contains_key(&core0));
    assert!(!state.partials.contains_key(&core1));
    assert!(state.prompts.contains_key(&core0.core));
    assert!(!state.prompts.contains_key(&core1.core));
}
#[test]
fn defmt_level_color_composes_after_channel_color() {
    let frame = DecodedFrame {
        message: "bad".into(),
        timestamp: None,
        level: Some(defmt_parser::Level::Error),
    };
    let mut output = Vec::new();
    let mut state = SessionState::new();
    state.channel_labels = true;
    state.color = true;

    render_defmt_frame(
        source(0, 1),
        &frame,
        Instant::now(),
        &mut state,
        None,
        &mut output,
    )
    .unwrap();

    assert_eq!(output, b"\x1b[35m[ch1] \x1b[0m\x1b[31merror bad\r\n\x1b[0m");
}

#[test]
fn reset_core_preserves_other_cores_state() {
    let core0 = source(0, 1);
    let core1 = source(1, 1);
    let mut state = SessionState::new();
    state.last_channel = Some(core1);
    state.partials.insert(core0, b"zero".to_vec());
    state.partials.insert(core1, b"one".to_vec());
    state.set_foreground(ForegroundLine { channel: core0, rendered: b"zero> ".to_vec() });
    state.prompts.insert(core1.core, ForegroundLine { channel: core1, rendered: b"one> ".to_vec() });
    state.reset_core(core1.core);
    assert!(state.partials.contains_key(&core0));
    assert!(!state.partials.contains_key(&core1));
    assert!(state.prompts.contains_key(&core0.core));
    assert!(!state.prompts.contains_key(&core1.core));
    assert_eq!(state.foreground().unwrap().channel, core0);
}

#[test]
fn reset_target_clears_all_presentation_state() {
    let mut state = SessionState::new();
    let src = source(0, 0);
    state.line_start = false;
    state.last_channel = Some(src);
    state.partials.insert(src, b"partial".to_vec());
    state.set_foreground(ForegroundLine { channel: src, rendered: b"> ".to_vec() });
    state.reset_target();
    assert!(state.line_start);
    assert!(state.last_channel.is_none());
    assert!(state.partials.is_empty());
    assert!(state.prompts.is_empty());
    assert!(state.foreground().is_none());
}

#[test]
fn selected_down_core_keeps_foreground_when_its_prompt_completes_or_clears() {
    let ch = ChannelId::from_cli(0, "test").unwrap();
    let selected = CoreChannel {
        core: CoreId::new(0),
        channel: ch,
    };
    let background = CoreChannel {
        core: CoreId::new(1),
        channel: ch,
    };
    let mut renderer = Renderer::new(Vec::new(), None, SessionState::new());
    renderer.select_down_core(Some(selected.core)).unwrap();
    let mut selected_feed = ChunkFeed::new();
    let mut background_feed = ChunkFeed::new();
    let now = Instant::now();

    let chunk = selected_feed.chunk(&renderer.state, ch, b"> ");
    renderer
        .render_terminal_event(selected, chunk, now)
        .unwrap();
    let chunk = selected_feed.chunk(&renderer.state, ch, b"done\n");
    renderer
        .render_terminal_event(selected, chunk, now)
        .unwrap();
    assert!(renderer.state.foreground().is_none());
    let before = renderer.output.len();
    let chunk = background_feed.chunk(&renderer.state, ch, b"background> ");
    renderer
        .render_terminal_event(background, chunk, now)
        .unwrap();
    assert_eq!(renderer.output.len(), before);
    assert_eq!(
        renderer.state.prompts[&background.core].rendered,
        b"background> "
    );

    let chunk = selected_feed.chunk(&renderer.state, ch, b"> ");
    renderer
        .render_terminal_event(selected, chunk, now)
        .unwrap();
    let chunk = selected_feed.chunk(&renderer.state, ch, b"\r\x1b[2K");
    renderer
        .render_terminal_event(selected, chunk, now)
        .unwrap();
    assert!(renderer.state.foreground().is_none());
    let before = renderer.output.len();
    let chunk = background_feed.chunk(&renderer.state, ch, b"still here");
    renderer
        .render_terminal_event(background, chunk, now)
        .unwrap();
    assert_eq!(renderer.output.len(), before);

    renderer.select_down_core(Some(background.core)).unwrap();
    assert!(renderer.show_cached_prompt(background.core).unwrap());
    assert_eq!(renderer.state.foreground().unwrap().channel, background);
}

#[test]
fn unavailable_down_core_releases_foreground_to_other_cores() {
    let ch = ChannelId::from_cli(0, "test").unwrap();
    let lost = CoreId::new(0);
    let healthy = CoreChannel {
        core: CoreId::new(1),
        channel: ch,
    };
    let mut renderer = Renderer::new(Vec::new(), None, SessionState::new());
    renderer.select_down_core(Some(lost)).unwrap();
    let mut feed = ChunkFeed::new();
    let now = Instant::now();

    let chunk = feed.chunk(&renderer.state, ch, b"healthy> ");
    renderer.render_terminal_event(healthy, chunk, now).unwrap();
    assert!(renderer.state.foreground().is_none());
    assert!(renderer.output.is_empty());

    renderer.select_down_core(None).unwrap();
    assert!(renderer.show_cached_prompt(healthy.core).unwrap());
    assert_eq!(renderer.state.foreground().unwrap().channel, healthy);
    assert_eq!(renderer.output, b"healthy> ");
}

#[test]
fn reset_core_drops_its_cached_prompt() {
    let core0 = CoreChannel {
        core: CoreId::new(0),
        channel: ChannelId::from_cli(0, "test").unwrap(),
    };
    let core1 = CoreChannel {
        core: CoreId::new(1),
        channel: ChannelId::from_cli(0, "test").unwrap(),
    };
    let mut state = SessionState::new();
    state.channel_labels = true;
    state.show_cores = true;
    let mut feed0 = ChunkFeed::new();
    let mut feed1 = ChunkFeed::new();
    let mut output = Vec::new();
    let timestamp = Instant::now();

    let chunk = feed0.chunk(&state, ChannelId::from_cli(0, "test").unwrap(), b"m7:~$ ");
    render_terminal_chunk(core0, chunk, timestamp, &mut state, &mut output).unwrap();
    let chunk = feed1.chunk(&state, ChannelId::from_cli(0, "test").unwrap(), b"m4:~$ ");
    render_terminal_chunk(core1, chunk, timestamp, &mut state, &mut output).unwrap();

    // A reattached core rebooted, so its cached prompt is stale.
    let mut renderer = Renderer::new(Vec::new(), None, state);
    let _ = renderer.suspend_foreground().unwrap();
    renderer.output.clear();
    renderer.state.reset_core(CoreId::new(1));
    assert!(!renderer.show_cached_prompt(CoreId::new(1)).unwrap());
    assert!(renderer.output.is_empty());
}

#[test]
fn erase_core_prompt_erases_only_that_cores_foreground() {
    let core0 = CoreChannel {
        core: CoreId::new(0),
        channel: ChannelId::from_cli(0, "test").unwrap(),
    };
    let mut state = SessionState::new();
    state.set_foreground(ForegroundLine {
        channel: core0,
        rendered: b"m7:~$ ".to_vec(),
    });
    let mut renderer = Renderer::new(Vec::new(), None, state);

    // Another core's prompt is untouched.
    renderer.erase_core_prompt(CoreId::new(1)).unwrap();
    assert_eq!(renderer.state.foreground().unwrap().channel, core0);

    // The owning core's prompt is erased and dropped.
    renderer.erase_core_prompt(CoreId::new(0)).unwrap();
    assert_eq!(renderer.output, [ERASE_CURRENT_LINE, ANSI_RESET].concat());
    assert!(renderer.state.foreground().is_none());
}

#[test]
fn reset_target_clears_renderer_state() {
    let mut state = SessionState::new();
    let src = source(0, 0);
    state.line_start = false;
    state.last_channel = Some(src);
    state.partials.insert(src, b"partial".to_vec());

    state.set_foreground(ForegroundLine {
        channel: src,
        rendered: b"> ".to_vec(),
    });

    state.reset_target();

    assert!(state.line_start);
    assert!(state.last_channel.is_none());
    assert!(state.partials.is_empty());
    assert!(state.prompts.is_empty());
    assert!(state.foreground().is_none());
}

#[test]
fn decoded_terminal_and_renderer_agree_on_redraw_semantics() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();
    let long = vec![b'a'; 4096];
    render_bytes_chunked(&mut state, &mut feed, channel(0), &long, Instant::now(), &mut output);
    render_bytes_chunked(&mut state, &mut feed, channel(0), b"\n", Instant::now(), &mut output);
    assert_eq!(output, [long, b"\n".to_vec()].concat());
}

#[test]
fn terminal_output_preserves_sgr_colors_without_cursor_rewrites() {
    let mut state = SessionState::new();
    let mut feed = ChunkFeed::new();
    let mut output = Vec::new();

    render_bytes_chunked(
        &mut state,
        &mut feed,
        ChannelId::from_cli(0, "test").unwrap(),
        b"\x1b[31mred\x1b[0m\n",
        Instant::now(),
        &mut output,
    );

    assert_eq!(output, b"\x1b[31mred\x1b[0m\r\n");
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
        false,
    )
    .unwrap()
    .unwrap();
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut renderer = Renderer::new(Vec::new(), Some(logger), state);
    let mut decoder = DecodedStream::new();
    let chunk = decoder.consume_chunk(b"old\r\x1b[2Knew\n", false);
    assert_eq!(chunk.lines[0].plain, b"new\n");
    renderer.render_terminal_event(source(0, 0), chunk, Instant::now()).unwrap();
    assert_eq!(renderer.output, b"new\n");
    renderer.finish_target_epoch().unwrap();
    drop(renderer);
    assert_eq!(fs::read(&path).unwrap(), b"new\n");
    fs::remove_file(path).unwrap();
}
