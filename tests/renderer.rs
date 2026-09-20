use super::*;
use crate::terminal::{DecodedStream, TerminalLine};
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

#[test]
fn timestamps_are_added_once_per_logical_line() {
    let mut state = SessionState::new();
    state.timestamps = true;

    let timestamp = state.started + Duration::from_millis(123);
    let src = source(0, 0);
    let mut output = Vec::new();

    render_channel_bytes(b"partial", src, timestamp, &mut state, &mut output, None).unwrap();

    render_channel_bytes(
        b" line\nnext",
        src,
        timestamp,
        &mut state,
        &mut output,
        None,
    )
    .unwrap();

    let wall =
        (state.started_wall + chrono::Duration::milliseconds(123)).format("%Y-%m-%d %H:%M:%S%.3f");

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

    assert_eq!(output, b"[ch2] first\r\n[ch2] second\r\n");
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

    assert_eq!(output, [b"styled".as_slice(), ANSI_RESET, b"\r\n"].concat());
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
fn redirected_defmt_frame_is_rendered() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;
    let mut renderer = Renderer::new(Vec::new(), None, state);
    let frame = DecodedFrame {
        message: "visible".into(),
        timestamp: None,
        level: Some(defmt_parser::Level::Trace),
    };

    renderer
        .render_defmt_frame(source(0, 0), &frame, Instant::now())
        .unwrap();

    assert_eq!(renderer.output, b"trace visible\n");
}

#[test]
fn redirected_partials_are_buffered_and_flushed_in_source_order() {
    let mut state = SessionState::new();
    state.presentation = Presentation::Redirected;

    let mut renderer = Renderer::new(Vec::new(), None, state);
    let timestamp = Instant::now();

    renderer
        .render_terminal_event(source(0, 2), partial(b"two", b"ignored"), timestamp)
        .unwrap();

    renderer
        .render_terminal_event(source(0, 0), partial(b"zero", b"ignored"), timestamp)
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
    assert_eq!(state.foreground().unwrap().channel, foreground_source);

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

    assert_eq!(state.foreground().unwrap().channel, foreground_source);
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

    render_terminal_chunk(
        src,
        partial(b"> ", b"> "),
        Instant::now(),
        &mut state,
        &mut output,
    )
    .unwrap();

    render_terminal_chunk(src, empty_chunk(), Instant::now(), &mut state, &mut output).unwrap();

    let expected = [b"> ".as_slice(), ERASE_CURRENT_LINE, ANSI_RESET].concat();

    assert_eq!(output, expected);
    assert!(state.foreground().is_none());
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

    let saved = erase_foreground(&mut state, &mut output).unwrap().unwrap();

    assert_eq!(saved.channel, src);
    assert_eq!(output, [ERASE_CURRENT_LINE, ANSI_RESET].concat());
    assert!(state.foreground().is_none());
}

#[test]
fn reset_core_removes_only_that_cores_cached_state() {
    let core0 = source(0, 0);
    let core1 = source(1, 0);

    let mut state = SessionState::new();

    state.partials.insert(core0, b"zero".to_vec());
    state.partials.insert(core1, b"one".to_vec());

    state.prompts.insert(
        core0.core,
        ForegroundLine {
            channel: core0,
            rendered: b"zero> ".to_vec(),
        },
    );

    state.prompts.insert(
        core1.core,
        ForegroundLine {
            channel: core1,
            rendered: b"one> ".to_vec(),
        },
    );

    state.set_foreground(ForegroundLine {
        channel: core0,
        rendered: b"zero> ".to_vec(),
    });

    state.reset_core(core1.core);

    assert!(state.partials.contains_key(&core0));
    assert!(!state.partials.contains_key(&core1));

    assert!(state.prompts.contains_key(&core0.core));
    assert!(!state.prompts.contains_key(&core1.core));

    assert_eq!(state.foreground().unwrap().channel, core0);
}

#[test]
fn reset_target_clears_all_presentation_state() {
    let src = source(0, 0);

    let mut state = SessionState::new();
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

    let mut decoder = DecodedStream::new();
    let mut output = Vec::new();

    let chunk = decoder.consume_chunk(b"old\r\x1b[2Knew\n", false);

    render_terminal_chunk(source(0, 0), chunk, Instant::now(), &mut state, &mut output).unwrap();

    assert_eq!(output, b"new\n");
}
