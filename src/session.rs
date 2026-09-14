use crate::cli::{ChannelMode, ChannelSpec};
use crate::defmt::{
    decode_frames, filter_level, level_enabled, level_name, DecodeOutput, DecodedFrame, DefmtData,
};
use crate::logger::Logger;
use anyhow::{bail, Context, Result};
use brtt::rtt::Rtt;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use probe_rs::Core;
use std::io::prelude::*;
use std::io::{stdout, IsTerminal};
use std::time::{Duration, Instant};

#[derive(Debug)]
struct ChannelEvent {
    channel_idx: u32,
    payload: ChannelPayload,
    timestamp: Instant,
}

#[derive(Debug)]
enum ChannelPayload {
    Bytes(Vec<u8>),
    Defmt(DecodedFrame),
    Warning(String),
}

struct UpChannelReader<'table> {
    spec: ChannelSpec,
    buffer: [u8; 128],
    decoder: Option<Box<dyn defmt_decoder::StreamDecoder + Send + Sync + 'table>>,
    defmt_can_recover: bool,
}

impl<'table> UpChannelReader<'table> {
    fn new(spec: ChannelSpec, defmt: Option<&'table DefmtData>) -> Result<Self> {
        let decoder = match (spec.mode, defmt) {
            (ChannelMode::Defmt, Some(defmt)) => Some(defmt.table.new_stream_decoder()),
            (ChannelMode::Defmt, None) => bail!("missing defmt table for channel {}", spec.index),
            _ => None,
        };
        Ok(Self {
            spec,
            buffer: [0; 128],
            decoder,
            defmt_can_recover: defmt.is_some_and(|defmt| defmt.table.encoding().can_recover()),
        })
    }
}

struct SessionState {
    timestamps: bool,
    local_echo: bool,
    line_start: bool,
    started: Instant,
    defmt_decode_warnings: u64,
    color: bool,
    channel_labels: bool,
    last_channel: Option<u32>,
    line_channel: Option<u32>,
    line_bytes: Vec<u8>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            timestamps: false,
            local_echo: false,
            line_start: true,
            started: Instant::now(),
            defmt_decode_warnings: 0,
            color: false,
            channel_labels: false,
            last_channel: None,
            line_channel: None,
            line_bytes: Vec::new(),
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
    readers: &mut [UpChannelReader<'_>],
    locations: Option<&defmt_decoder::Locations>,
    logger: Option<&mut Logger>,
) -> Result<Vec<ChannelEvent>> {
    let mut logger = logger;
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
            let timestamp = Instant::now();
            if let Some(logger) = logger.as_deref_mut() {
                logger.write_raw(reader.spec.index, &reader.buffer[..count])?;
            }
            match reader.spec.mode {
                ChannelMode::Ascii => events.push(ChannelEvent {
                    channel_idx: reader.spec.index,
                    payload: ChannelPayload::Bytes(reader.buffer[..count].to_vec()),
                    timestamp,
                }),
                ChannelMode::Defmt => {
                    let frames = decode_frames(
                        &mut **reader.decoder.as_mut().expect("defmt decoder initialized"),
                        &reader.buffer[..count],
                        locations,
                        reader.defmt_can_recover,
                    )
                    .with_context(|| {
                        format!("failed to decode defmt on up channel {}", reader.spec.index)
                    })?;
                    events.extend(frames.into_iter().map(|output| ChannelEvent {
                        channel_idx: reader.spec.index,
                        payload: match output {
                            DecodeOutput::Frame(frame) => ChannelPayload::Defmt(frame),
                            DecodeOutput::Warning(warning) => ChannelPayload::Warning(warning),
                        },
                        timestamp,
                    }));
                }
            }
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
    render_channel_bytes_colored(bytes, None, timestamp, state, output, None)
}

fn render_channel_bytes(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    render_channel_bytes_colored(bytes, channel_idx, timestamp, state, output, None)
}

fn render_channel_bytes_colored(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
    line_color: Option<&'static str>,
) -> std::io::Result<()> {
    let suspended = if !state.line_start && state.line_channel != channel_idx {
        let saved = PartialLine {
            bytes: std::mem::take(&mut state.line_bytes),
            channel_idx: state.line_channel.or(state.last_channel),
            timestamp: Instant::now(),
        };
        output.write_all(b"\r\x1b[2K")?;
        state.line_start = true;
        state.line_channel = None;
        state.last_channel = None;
        Some(saved)
    } else {
        None
    };

    render_channel_bytes_colored_inner(bytes, channel_idx, timestamp, state, output, line_color)?;

    if let Some(saved) = suspended {
        state.line_start = true;
        state.last_channel = None;
        render_channel_bytes_colored_inner(
            &saved.bytes,
            saved.channel_idx,
            saved.timestamp,
            state,
            output,
            None,
        )?;
    }
    Ok(())
}

struct PartialLine {
    bytes: Vec<u8>,
    channel_idx: Option<u32>,
    timestamp: Instant,
}

fn suspend_partial_line(
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<Option<PartialLine>> {
    if state.line_start {
        return Ok(None);
    }

    let saved = PartialLine {
        bytes: std::mem::take(&mut state.line_bytes),
        channel_idx: state.line_channel.or(state.last_channel),
        timestamp: Instant::now(),
    };
    output.write_all(b"\r\x1b[2K")?;
    state.line_start = true;
    state.line_channel = None;
    state.last_channel = None;
    Ok(Some(saved))
}

fn render_channel_bytes_colored_inner(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
    line_color: Option<&'static str>,
) -> std::io::Result<()> {
    if !bytes.is_empty() {
        state.line_channel = channel_idx;
    }
    for &byte in bytes {
        let line_start = state.line_start;
        let channel_switch =
            channel_idx.is_some() && state.last_channel != channel_idx && state.channel_labels;
        if state.timestamps && line_start {
            let elapsed = timestamp.saturating_duration_since(state.started);
            write!(output, "[+{:>8.3}s] ", elapsed.as_secs_f64())?;
        }

        if state.channel_labels && (line_start || channel_switch) {
            if let Some(channel_idx) = channel_idx {
                if state.color {
                    write!(output, "{}", channel_color(channel_idx))?;
                }
                write!(output, "[ch{channel_idx}] ")?;
                if state.color {
                    output.write_all(b"\x1b[0m")?;
                }
                state.last_channel = Some(channel_idx);
            }
        }

        if let Some(line_color) = line_color {
            if line_start || channel_switch {
                output.write_all(line_color.as_bytes())?;
            }
        }

        if line_start {
            state.line_start = false;
        }

        if byte == b'\n' {
            output.write_all(b"\r")?;
            state.line_start = true;
        } else {
            state.line_start = false;
        }
        output.write_all(&[byte])?;
        if byte != b'\n' {
            state.line_bytes.push(byte);
        } else {
            state.line_bytes.clear();
            state.line_channel = None;
        }
        if byte == b'\n' && line_color.is_some() {
            output.write_all(b"\x1b[0m")?;
        }
    }

    if line_color.is_some() && !state.line_start {
        output.write_all(b"\x1b[0m")?;
    }

    Ok(())
}

fn channel_color(channel_idx: u32) -> &'static str {
    [
        "\x1b[36m", "\x1b[35m", "\x1b[34m", "\x1b[32m", "\x1b[33m", "\x1b[31m",
    ][channel_idx as usize % 6]
}

fn render_events(
    events: &[ChannelEvent],
    state: &mut SessionState,
    filters: Option<&[(String, defmt_parser::Level)]>,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let mut logger = logger;
    for event in events {
        match &event.payload {
            ChannelPayload::Bytes(bytes) => {
                if let Some(logger) = logger.as_deref_mut() {
                    logger
                        .write_decoded(event.channel_idx, bytes, state.channel_labels)
                        .map_err(io_error)?;
                }
                render_channel_bytes(
                    bytes,
                    Some(event.channel_idx),
                    event.timestamp,
                    state,
                    output,
                )?
            }
            ChannelPayload::Defmt(frame) => {
                if let (Some(level), Some(filters)) = (frame.level, filters) {
                    let minimum = filter_level(frame.module.as_deref(), filters);
                    if !level_enabled(level, minimum) {
                        continue;
                    }
                }
                let mut line = String::new();
                if let Some(timestamp) = &frame.timestamp {
                    line.push('[');
                    line.push_str(timestamp);
                    line.push_str("] ");
                }
                if let Some(level) = frame.level {
                    line.push_str(level_name(level));
                    line.push(' ');
                }
                line.push_str(&frame.message);
                line.push('\n');
                if let Some(logger) = logger.as_deref_mut() {
                    logger
                        .write_decoded(event.channel_idx, line.as_bytes(), state.channel_labels)
                        .map_err(io_error)?;
                }
                let level_color = if state.color {
                    let color = defmt_level_color(frame.level);
                    (!color.is_empty()).then_some(color)
                } else {
                    None
                };
                render_channel_bytes_colored(
                    line.as_bytes(),
                    Some(event.channel_idx),
                    event.timestamp,
                    state,
                    output,
                    level_color,
                )?;
            }
            ChannelPayload::Warning(warning) => {
                state.defmt_decode_warnings += 1;
                let line = format!(
                    "[defmt warning #{}] {warning}\n",
                    state.defmt_decode_warnings
                );
                if let Some(logger) = logger.as_deref_mut() {
                    logger
                        .write_decoded(event.channel_idx, line.as_bytes(), state.channel_labels)
                        .map_err(io_error)?;
                }
                render_channel_bytes(
                    line.as_bytes(),
                    Some(event.channel_idx),
                    event.timestamp,
                    state,
                    output,
                )?;
            }
        }
    }

    output.flush()
}

fn defmt_level_color(level: Option<defmt_parser::Level>) -> &'static str {
    match level {
        Some(defmt_parser::Level::Error) => "\x1b[31m",
        Some(defmt_parser::Level::Warn) => "\x1b[33m",
        Some(defmt_parser::Level::Debug | defmt_parser::Level::Trace) => "\x1b[2m",
        _ => "",
    }
}

fn io_error(error: anyhow::Error) -> std::io::Error {
    std::io::Error::other(error)
}

fn write_help(output: &mut impl Write) -> std::io::Result<()> {
    write!(
        output,
        "\r\nCtrl-T commands:\r\n  q  Quit\r\n  ?  Show this help\r\n  c  Show configuration\r\n  l  Clear screen\r\n  t  Toggle timestamps\r\n  e  Toggle local echo\r\n  R  Reset target\r\n  Ctrl-T  Send a literal Ctrl-T\r\n"
    )?;
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

fn interactive_input_available(has_down_channel: bool) -> bool {
    has_down_channel && std::io::stdin().is_terminal()
}

fn dispatch_command(
    command: SessionCommand,
    core: &mut Core,
    rtt: &mut Rtt,
    config: &SessionConfig,
    state: &mut SessionState,
    output: &mut impl Write,
) -> Result<bool> {
    if command == SessionCommand::Quit {
        return Ok(true);
    }
    let suspended = suspend_partial_line(state, output)?;
    match command {
        SessionCommand::Quit => unreachable!("quit handled before rendering command output"),
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

    if let Some(saved) = suspended {
        state.line_start = true;
        state.last_channel = None;
        render_channel_bytes_colored_inner(
            &saved.bytes,
            saved.channel_idx,
            saved.timestamp,
            state,
            output,
            None,
        )?;
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
    pub(crate) defmt: Option<DefmtData>,
    pub(crate) defmt_filters: Option<Vec<(String, defmt_parser::Level)>>,
    pub(crate) color: crate::cli::ColorMode,
    pub(crate) log: Option<std::path::PathBuf>,
    pub(crate) log_per_channel: bool,
    pub(crate) log_format: crate::cli::LogFormat,
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
    let defmt_ref = config.defmt.as_ref();
    let mut up_readers = config
        .up_specs
        .iter()
        .copied()
        .map(|spec| UpChannelReader::new(spec, defmt_ref))
        .collect::<Result<Vec<_>>>()?;

    if config.down_configured && rtt.down_channel(config.down_channel).is_none() {
        bail!(
            "Error: down channel {} does not exist.",
            config.down_channel
        );
    }

    let mut logger = Logger::new(
        config.log.as_deref(),
        config.log_per_channel,
        config.log_format,
        config.up_specs.len(),
    )?;

    if config.reset {
        core.reset()?;
    }

    let mut down_buf = Vec::new();
    let mut escape_state = EscapeState::Normal;
    let mut state = SessionState::new();
    state.channel_labels = config.up_specs.len() > 1;
    state.color = match config.color {
        crate::cli::ColorMode::Always => true,
        crate::cli::ColorMode::Never => false,
        crate::cli::ColorMode::Auto => std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };
    let stdin_setup = interactive_input_available(rtt.down_channel(config.down_channel).is_some());

    let _raw_mode = if stdin_setup {
        terminal::enable_raw_mode()?;
        Some(RawModeGuard)
    } else {
        None
    };

    let mut output = stdout();
    let result = 'read_loop: loop {
        let events = match poll_up_channels(
            &mut rtt,
            core,
            &mut up_readers,
            config
                .defmt
                .as_ref()
                .and_then(|defmt| defmt.locations.as_ref()),
            logger.as_mut(),
        ) {
            Ok(events) => events,
            Err(err) => {
                break 'read_loop Err(anyhow::anyhow!("\nError reading from RTT: {err}"));
            }
        };

        if !events.is_empty() {
            if let Err(err) = render_events(
                &events,
                &mut state,
                config.defmt_filters.as_deref(),
                logger.as_mut(),
                &mut output,
            ) {
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

    if let Some(logger) = &mut logger {
        logger.flush()?;
    }
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
            defmt: None,
            defmt_filters: None,
            color: crate::cli::ColorMode::Never,
            log: None,
            log_per_channel: false,
            log_format: crate::cli::LogFormat::Decoded,
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
    fn non_terminal_input_does_not_block_headless_output() {
        assert!(!interactive_input_available(false));
    }

    #[test]
    fn channel_events_keep_tags_and_render_in_order() {
        let events = vec![
            ChannelEvent {
                channel_idx: 2,
                payload: ChannelPayload::Bytes(b"log\n".to_vec()),
                timestamp: Instant::now(),
            },
            ChannelEvent {
                channel_idx: 0,
                payload: ChannelPayload::Bytes(b"shell".to_vec()),
                timestamp: Instant::now(),
            },
        ];
        let mut output = Vec::new();
        let mut state = SessionState::new();

        render_events(&events, &mut state, None, None, &mut output).unwrap();

        assert_eq!(events[0].channel_idx, 2);
        assert_eq!(events[1].channel_idx, 0);
        assert_eq!(output, b"log\r\nshell");
    }

    #[test]
    fn multiple_channels_are_labeled_on_each_line() {
        let events = vec![
            ChannelEvent {
                channel_idx: 0,
                payload: ChannelPayload::Bytes(b"zero\none".to_vec()),
                timestamp: Instant::now(),
            },
            ChannelEvent {
                channel_idx: 1,
                payload: ChannelPayload::Bytes(b"one\n".to_vec()),
                timestamp: Instant::now(),
            },
        ];
        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;

        render_events(&events, &mut state, None, None, &mut output).unwrap();

        assert_eq!(
            output,
            b"[ch0] zero\r\n[ch0] one\r\x1b[2K[ch1] one\r\n[ch0] one"
        );
    }

    #[test]
    fn channel_labels_use_stable_palette_colors() {
        assert_eq!(channel_color(0), "\x1b[36m");
        assert_eq!(channel_color(6), channel_color(0));

        let events = vec![ChannelEvent {
            channel_idx: 1,
            payload: ChannelPayload::Bytes(b"line\n".to_vec()),
            timestamp: Instant::now(),
        }];
        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;
        state.color = true;

        render_events(&events, &mut state, None, None, &mut output).unwrap();

        assert_eq!(output, b"\x1b[35m[ch1] \x1b[0mline\r\n");
    }

    #[test]
    fn defmt_level_color_composes_after_channel_color() {
        let events = vec![ChannelEvent {
            channel_idx: 1,
            payload: ChannelPayload::Defmt(DecodedFrame {
                message: "bad".to_string(),
                timestamp: None,
                level: Some(defmt_parser::Level::Error),
                module: None,
            }),
            timestamp: Instant::now(),
        }];
        let mut output = Vec::new();
        let mut state = SessionState::new();
        state.channel_labels = true;
        state.color = true;

        render_events(&events, &mut state, None, None, &mut output).unwrap();

        assert_eq!(output, b"\x1b[35m[ch1] \x1b[0m\x1b[31merror bad\r\n\x1b[0m");
    }
}
