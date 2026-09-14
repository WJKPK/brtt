use crate::cli::{ChannelMode, ChannelSpec};
use anyhow::{bail, Context, Result};
use brtt::rtt::Rtt;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use probe_rs::Core;
use std::io::prelude::*;
use std::io::stdout;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct ChannelEvent {
    channel_idx: u32,
    mode: ChannelMode,
    bytes: Vec<u8>,
    timestamp: Instant,
}

struct UpChannelReader {
    spec: ChannelSpec,
    buffer: [u8; 128],
}

impl UpChannelReader {
    fn new(spec: ChannelSpec) -> Self {
        Self {
            spec,
            buffer: [0; 128],
        }
    }
}

struct SessionState {
    timestamps: bool,
    local_echo: bool,
    line_start: bool,
    started: Instant,
}

impl SessionState {
    fn new() -> Self {
        Self {
            timestamps: false,
            local_echo: false,
            line_start: true,
            started: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Normal,
    AwaitingCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionCommand {
    Quit,
    Help,
    ShowConfig,
    ClearScreen,
    ToggleTimestamps,
    ToggleLocalEcho,
    ResetTarget,
}

#[derive(Debug, PartialEq, Eq)]
enum InputAction {
    Send(Vec<u8>),
    Command(SessionCommand),
    Ignore,
}

impl EscapeState {
    fn handle_key(self, key: KeyEvent) -> (Self, InputAction) {
        if key.kind != KeyEventKind::Press {
            return (self, InputAction::Ignore);
        }

        if is_control_key(key, 'c') {
            return (
                EscapeState::Normal,
                InputAction::Command(SessionCommand::Quit),
            );
        }

        match self {
            EscapeState::Normal if is_control_key(key, 't') => {
                (EscapeState::AwaitingCommand, InputAction::Ignore)
            }
            EscapeState::Normal => (EscapeState::Normal, key_to_action(key)),
            EscapeState::AwaitingCommand => {
                if is_control_key(key, 't') {
                    return (EscapeState::Normal, InputAction::Send(vec![0x14]));
                }

                match key.code {
                    KeyCode::Char('q') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::Quit),
                    ),
                    KeyCode::Char('?') => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::Help),
                    ),
                    KeyCode::Char('c') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ShowConfig),
                    ),
                    KeyCode::Char('l') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ClearScreen),
                    ),
                    KeyCode::Char('t') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ToggleTimestamps),
                    ),
                    KeyCode::Char('e') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ToggleLocalEcho),
                    ),
                    KeyCode::Char('R') => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ResetTarget),
                    ),
                    _ => (EscapeState::Normal, key_to_action(key)),
                }
            }
        }
    }
}

fn is_control_key(key: KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers == KeyModifiers::CONTROL
}

fn key_to_action(key: KeyEvent) -> InputAction {
    let mut bytes = Vec::new();

    match key.code {
        KeyCode::Char(c) => bytes.extend_from_slice(c.to_string().as_bytes()),
        KeyCode::Enter => bytes.push(b'\n'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Backspace => bytes.push(8u8),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        _ => {}
    }

    if bytes.is_empty() {
        InputAction::Ignore
    } else {
        InputAction::Send(bytes)
    }
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec]) -> Result<()> {
    for spec in specs {
        let channel = usize::try_from(spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                spec.index
            )
        })?;

        if rtt.up_channel(channel).is_none() {
            bail!("Error: up channel {} does not exist.", spec.index);
        }
    }

    Ok(())
}

fn poll_up_channels(
    rtt: &mut Rtt,
    core: &mut Core,
    readers: &mut [UpChannelReader],
) -> Result<Vec<ChannelEvent>> {
    let mut events = Vec::with_capacity(readers.len());

    for reader in readers {
        let channel = usize::try_from(reader.spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                reader.spec.index
            )
        })?;

        let count = match rtt.up_channel(channel) {
            Some(channel) => channel.read(core, &mut reader.buffer)?,
            None => continue,
        };

        if count > 0 {
            events.push(ChannelEvent {
                channel_idx: reader.spec.index,
                mode: reader.spec.mode,
                bytes: reader.buffer[..count].to_vec(),
                timestamp: Instant::now(),
            });
        }
    }

    events.sort_by_key(|event| event.timestamp);
    Ok(events)
}

fn render_bytes(
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    for &byte in bytes {
        if state.timestamps && state.line_start {
            let elapsed = timestamp.saturating_duration_since(state.started);
            write!(output, "[+{:>8.3}s] ", elapsed.as_secs_f64())?;
            state.line_start = false;
        }

        if byte == b'\n' {
            output.write_all(b"\r")?;
            state.line_start = true;
        } else {
            state.line_start = false;
        }
        output.write_all(&[byte])?;
    }

    Ok(())
}

fn render_events(
    events: &[ChannelEvent],
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    for event in events {
        let _ = event.channel_idx;
        match event.mode {
            ChannelMode::Ascii => {
                render_bytes(&event.bytes, event.timestamp, state, output)?;
            }
        }
    }

    output.flush()
}

fn write_help(output: &mut impl Write) -> std::io::Result<()> {
    writeln!(output, "\r\nCtrl-T commands:")?;
    writeln!(output, "  q  Quit")?;
    writeln!(output, "  ?  Show this help")?;
    writeln!(output, "  c  Show configuration")?;
    writeln!(output, "  l  Clear screen")?;
    writeln!(output, "  t  Toggle timestamps")?;
    writeln!(output, "  e  Toggle local echo")?;
    writeln!(output, "  R  Reset target")?;
    writeln!(output, "  Ctrl-T  Send a literal Ctrl-T")?;
    output.flush()
}

fn write_config(
    config: &SessionConfig,
    state: &SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    writeln!(output, "\r\nConfiguration:")?;
    writeln!(output, "  Probe: {}", config.probe)?;
    writeln!(output, "  Chip: {}", config.chip)?;
    write!(output, "  Up channels:")?;
    for spec in &config.up_specs {
        write!(output, " {}:{}", spec.index, spec.mode.name())?;
    }
    writeln!(output)?;
    writeln!(output, "  Down channel: {}", config.down_channel)?;
    writeln!(
        output,
        "  Poll interval: {} ms",
        config.poll_interval.as_millis()
    )?;
    writeln!(output, "  Timestamps: {}", on_or_off(state.timestamps))?;
    writeln!(output, "  Local echo: {}", on_or_off(state.local_echo))?;
    output.flush()
}

fn clear_screen(output: &mut impl Write) -> std::io::Result<()> {
    execute!(
        output,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    output.flush()
}

fn on_or_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

fn dispatch_command(
    command: SessionCommand,
    core: &mut Core,
    rtt: &mut Rtt,
    config: &SessionConfig,
    state: &mut SessionState,
    output: &mut impl Write,
) -> Result<bool> {
    match command {
        SessionCommand::Quit => return Ok(true),
        SessionCommand::Help => {
            write_help(output)?;
            state.line_start = true;
        }
        SessionCommand::ShowConfig => {
            write_config(config, state, output)?;
            state.line_start = true;
        }
        SessionCommand::ClearScreen => {
            clear_screen(output)?;
            state.line_start = true;
        }
        SessionCommand::ToggleTimestamps => {
            state.timestamps = !state.timestamps;
            writeln!(output, "\r\nTimestamps: {}", on_or_off(state.timestamps))?;
            output.flush()?;
            state.line_start = true;
        }
        SessionCommand::ToggleLocalEcho => {
            state.local_echo = !state.local_echo;
            writeln!(output, "\r\nLocal echo: {}", on_or_off(state.local_echo))?;
            output.flush()?;
            state.line_start = true;
        }
        SessionCommand::ResetTarget => {
            core.reset().context("Error resetting target")?;
            // The target reset may rewind RTT pointers while the host retains old read state.
            rtt.reset_read_state();
            writeln!(output, "\r\nTarget reset.")?;
            output.flush()?;
            state.line_start = true;
        }
    }

    Ok(false)
}

pub(crate) struct SessionConfig {
    pub(crate) probe: String,
    pub(crate) chip: String,
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) up_configured: bool,
    pub(crate) down_channel: usize,
    pub(crate) down_configured: bool,
    pub(crate) poll_interval: Duration,
    pub(crate) reset: bool,
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

pub(crate) fn run_session(core: &mut Core, mut rtt: Rtt, config: SessionConfig) -> Result<()> {
    if config.up_configured {
        validate_up_specs(&mut rtt, &config.up_specs)?;
    }
    let mut up_readers = config
        .up_specs
        .iter()
        .copied()
        .map(UpChannelReader::new)
        .collect::<Vec<_>>();

    if config.down_configured && rtt.down_channel(config.down_channel).is_none() {
        bail!(
            "Error: down channel {} does not exist.",
            config.down_channel
        );
    }

    if config.reset {
        core.reset()?;
    }

    let mut down_buf = Vec::new();
    let mut escape_state = EscapeState::Normal;
    let mut state = SessionState::new();
    let stdin_setup = rtt.down_channel(config.down_channel).is_some();

    let _raw_mode = if stdin_setup {
        terminal::enable_raw_mode()?;
        Some(RawModeGuard)
    } else {
        None
    };

    let mut output = stdout();
    let result = 'read_loop: loop {
        let events = match poll_up_channels(&mut rtt, core, &mut up_readers) {
            Ok(events) => events,
            Err(err) => {
                break 'read_loop Err(anyhow::anyhow!("\nError reading from RTT: {err}"));
            }
        };

        if !events.is_empty() {
            if let Err(err) = render_events(&events, &mut state, &mut output) {
                break 'read_loop Err(anyhow::anyhow!("Error writing to stdout: {err}"));
            }
        }

        if stdin_setup && event::poll(config.poll_interval)? {
            if let Event::Key(key_event) = event::read()? {
                let (next_state, action) = escape_state.handle_key(key_event);
                escape_state = next_state;

                match action {
                    InputAction::Send(bytes) => {
                        if state.local_echo {
                            if let Err(err) =
                                render_bytes(&bytes, Instant::now(), &mut state, &mut output)
                            {
                                break 'read_loop Err(anyhow::anyhow!(
                                    "Error writing local echo: {err}"
                                ));
                            }
                        }
                        down_buf.extend_from_slice(&bytes);
                    }
                    InputAction::Command(command) => {
                        match dispatch_command(
                            command,
                            core,
                            &mut rtt,
                            &config,
                            &mut state,
                            &mut output,
                        ) {
                            Ok(true) => break 'read_loop Ok(()),
                            Ok(false) => {}
                            Err(err) => break 'read_loop Err(err),
                        }
                    }
                    InputAction::Ignore => {}
                }
            }
        } else if !stdin_setup {
            std::thread::sleep(config.poll_interval);
        }

        if let Some(down_channel) = rtt.down_channel(config.down_channel) {
            if !down_buf.is_empty() {
                let count = match down_channel.write(core, down_buf.as_mut()) {
                    Ok(count) => count,
                    Err(err) => {
                        break 'read_loop Err(anyhow::anyhow!("\nError writing to RTT: {err}"));
                    }
                };

                if count > 0 {
                    down_buf.drain(..count);
                }
            }
        }
    };

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn ctrl_t_enters_command_mode_without_sending() {
        let (state, action) =
            EscapeState::Normal.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert_eq!(state, EscapeState::AwaitingCommand);
        assert_eq!(action, InputAction::Ignore);
    }

    #[test]
    fn ctrl_t_q_quits() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('q'), KeyModifiers::NONE));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Command(SessionCommand::Quit));
    }

    #[test]
    fn ctrl_t_question_requests_help() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('?'), KeyModifiers::SHIFT));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Command(SessionCommand::Help));
    }

    #[test]
    fn ctrl_t_core_commands_dispatch_to_their_commands() {
        for (character, command) in [
            ('c', SessionCommand::ShowConfig),
            ('l', SessionCommand::ClearScreen),
            ('t', SessionCommand::ToggleTimestamps),
            ('e', SessionCommand::ToggleLocalEcho),
        ] {
            assert_eq!(
                EscapeState::AwaitingCommand
                    .handle_key(key(KeyCode::Char(character), KeyModifiers::NONE)),
                (EscapeState::Normal, InputAction::Command(command))
            );
        }

        assert_eq!(
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('R'), KeyModifiers::SHIFT)),
            (
                EscapeState::Normal,
                InputAction::Command(SessionCommand::ResetTarget)
            )
        );
    }

    #[test]
    fn ctrl_t_ctrl_t_sends_literal_ctrl_t() {
        let (state, action) =
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('t'), KeyModifiers::CONTROL));

        assert_eq!(state, EscapeState::Normal);
        assert_eq!(action, InputAction::Send(vec![0x14]));
    }

    #[test]
    fn ordinary_keys_keep_existing_encoding() {
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(vec![b'\n']))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Up, KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(b"\x1b[A".to_vec()))
        );
    }

    #[test]
    fn unknown_command_keys_pass_through_and_reset_state() {
        assert_eq!(
            EscapeState::AwaitingCommand.handle_key(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
        );
    }

    #[test]
    fn key_releases_do_not_change_escape_state() {
        let mut released = key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        released.kind = KeyEventKind::Release;

        assert_eq!(
            EscapeState::Normal.handle_key(released),
            (EscapeState::Normal, InputAction::Ignore)
        );
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
    }

    #[test]
    fn timestamps_are_added_once_per_line_across_partial_events() {
        let mut state = SessionState::new();
        state.timestamps = true;
        let timestamp = state.started + Duration::from_millis(123);
        let mut output = Vec::new();

        render_bytes(b"partial", timestamp, &mut state, &mut output).unwrap();
        render_bytes(b" line\nnext", timestamp, &mut state, &mut output).unwrap();

        assert_eq!(output, b"[+   0.123s] partial line\r\n[+   0.123s] next");
    }

    #[test]
    fn timestamps_are_disabled_by_default() {
        let mut state = SessionState::new();
        let mut output = Vec::new();

        render_bytes(b"text\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output, b"text\r\n");
    }

    #[test]
    fn config_and_clear_screen_outputs_include_session_settings() {
        let config = SessionConfig {
            probe: "probe-id".to_string(),
            chip: "nRF52840_xxAA".to_string(),
            up_specs: vec![ChannelSpec {
                index: 2,
                mode: ChannelMode::Ascii,
            }],
            up_configured: true,
            down_channel: 1,
            down_configured: true,
            poll_interval: Duration::from_millis(10),
            reset: false,
        };
        let state = SessionState::new();
        let mut output = Vec::new();

        write_config(&config, &state, &mut output).unwrap();
        let config_output = String::from_utf8(output).unwrap();
        assert!(config_output.contains("Probe: probe-id"));
        assert!(config_output.contains("Chip: nRF52840_xxAA"));
        assert!(config_output.contains("Up channels: 2:ascii"));
        assert!(config_output.contains("Down channel: 1"));
        assert!(config_output.contains("Poll interval: 10 ms"));

        let mut clear_output = Vec::new();
        clear_screen(&mut clear_output).unwrap();
        assert!(clear_output.starts_with(b"\x1b[2J"));
    }

    #[test]
    fn channel_events_keep_tags_and_render_in_order() {
        let events = vec![
            ChannelEvent {
                channel_idx: 2,
                mode: ChannelMode::Ascii,
                bytes: b"log\n".to_vec(),
                timestamp: Instant::now(),
            },
            ChannelEvent {
                channel_idx: 0,
                mode: ChannelMode::Ascii,
                bytes: b"shell".to_vec(),
                timestamp: Instant::now(),
            },
        ];
        let mut output = Vec::new();
        let mut state = SessionState::new();

        render_events(&events, &mut state, &mut output).unwrap();

        assert_eq!(events[0].channel_idx, 2);
        assert_eq!(events[1].channel_idx, 0);
        assert_eq!(output, b"log\r\nshell");
    }
}
