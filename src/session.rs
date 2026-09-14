use crate::cli::{ChannelEncoding, ChannelSpec};
use crate::defmt::{
    decode_frames, filter_level, level_enabled, level_name, DecodeOutput, DecodedFrame, DefmtData,
};
use crate::logger::Logger;
use anyhow::{bail, Context, Result};
use brtt::rtt::{try_attach_to_rtt, try_attach_to_rtt_incremental, Rtt, ScanRegion};
use brtt::RttChannel;
use chrono::{DateTime, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use probe_rs::Core;
use std::collections::HashMap;
use std::io::prelude::*;
use std::io::{stdout, IsTerminal};
use std::time::{Duration, Instant};

const RTT_REATTACH_TIMEOUT: Duration = Duration::from_secs(3);
const TARGET_HALT_TIMEOUT: Duration = Duration::from_millis(100);

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
    buffer: [u8; 256],
    decoder: Option<Box<dyn defmt_decoder::StreamDecoder + Send + Sync + 'table>>,
    defmt_can_recover: bool,
}

impl<'table> UpChannelReader<'table> {
    fn new(spec: ChannelSpec, defmt: Option<&'table DefmtData>) -> Result<Self> {
        let decoder = match (spec.mode, defmt) {
            (ChannelEncoding::Defmt, Some(defmt)) => Some(defmt.table.new_stream_decoder()),
            (ChannelEncoding::Defmt, None) => {
                bail!("missing defmt table for channel {}", spec.index)
            }
            _ => None,
        };
        Ok(Self {
            spec,
            buffer: [0; 256],
            decoder,
            defmt_can_recover: defmt.is_some_and(|defmt| defmt.table.encoding().can_recover()),
        })
    }

    fn reset(&mut self, defmt: Option<&'table DefmtData>) -> Result<()> {
        *self = Self::new(self.spec, defmt)?;
        Ok(())
    }
}

struct SessionState {
    timestamps: bool,
    local_echo: bool,
    line_start: bool,
    started: Instant,
    started_wall: DateTime<Local>,
    defmt_decode_warnings: u64,
    color: bool,
    channel_labels: bool,
    last_channel: Option<u32>,
    streams: HashMap<u32, TextStream>,
    foreground: Option<ForegroundLine>,
    interactive: bool,
}

struct TextStream {
    bytes: Vec<u8>,
    pending_cr: bool,
}

struct ForegroundLine {
    channel: u32,
    bytes: Vec<u8>,
}

impl SessionState {
    fn new() -> Self {
        Self {
            timestamps: false,
            local_echo: false,
            line_start: true,
            started: Instant::now(),
            started_wall: Local::now(),
            defmt_decode_warnings: 0,
            color: false,
            channel_labels: false,
            last_channel: None,
            streams: HashMap::new(),
            foreground: None,
            interactive: true,
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
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return (self, InputAction::Ignore);
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
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if c.is_ascii_alphabetic() {
                bytes.push((c.to_ascii_lowercase() as u8) & 0x1f);
            }
        }
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => {
            bytes.push(0x1b);
            bytes.extend_from_slice(c.to_string().as_bytes());
        }
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

fn channel_by_number<T: RttChannel>(channels: &mut [T], number: usize) -> Option<&mut T> {
    channels
        .iter_mut()
        .find(|channel| channel.number() == number)
}

fn validate_up_specs(rtt: &mut Rtt, specs: &[ChannelSpec]) -> Result<()> {
    for spec in specs {
        let channel = usize::try_from(spec.index).with_context(|| {
            format!(
                "up channel index {} cannot be represented on this host",
                spec.index
            )
        })?;

        if channel_by_number(rtt.up_channels(), channel).is_none() {
            bail!("Error: up channel {} does not exist.", spec.index);
        }
    }

    Ok(())
}

fn validate_channels(rtt: &mut Rtt, config: &SessionConfig) -> Result<()> {
    validate_up_specs(rtt, &config.up_specs)?;
    if config.down_configured
        && channel_by_number(rtt.down_channels(), config.down_channel).is_none()
    {
        bail!(
            "Error: down channel {} does not exist.",
            config.down_channel
        );
    }
    Ok(())
}

fn reset_and_reattach(
    core: &mut Core,
    rtt: &mut Rtt,
    scan_region: &ScanRegion,
    automatic_scan: bool,
) -> Result<()> {
    core.halt(TARGET_HALT_TIMEOUT)
        .context("Error halting target before reset")?;
    Rtt::clear_control_block(core, &ScanRegion::Exact(rtt.ptr()))
        .context("Error clearing stale RTT control block before reset")?;
    core.reset().context("Error resetting target")?;
    *rtt = if automatic_scan {
        try_attach_to_rtt_incremental(core, RTT_REATTACH_TIMEOUT, scan_region)
    } else {
        try_attach_to_rtt(core, RTT_REATTACH_TIMEOUT, scan_region)
    }
    .context("Error reattaching to RTT after target reset")?;
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

        let count = match channel_by_number(rtt.up_channels(), channel) {
            Some(channel) => channel.read(core, &mut reader.buffer)?,
            None => continue,
        };

        if count > 0 {
            let timestamp = Instant::now();
            if let Some(logger) = logger.as_deref_mut() {
                logger.write_raw(reader.spec.index, &reader.buffer[..count])?;
            }
            match reader.spec.mode {
                ChannelEncoding::Terminal => events.push(ChannelEvent {
                    channel_idx: reader.spec.index,
                    payload: ChannelPayload::Bytes(reader.buffer[..count].to_vec()),
                    timestamp,
                }),
                ChannelEncoding::Defmt => {
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
    render_channel_bytes_colored_inner(bytes, channel_idx, timestamp, state, output, line_color)?;
    Ok(())
}

fn erase_foreground(
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<Option<ForegroundLine>> {
    let foreground = state.foreground.take();
    if foreground.is_some() && state.interactive {
        output.write_all(b"\r\x1b[2K")?;
    }
    state.line_start = true;
    state.last_channel = None;
    Ok(foreground)
}

fn render_channel_bytes_colored_inner(
    bytes: &[u8],
    channel_idx: Option<u32>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
    line_color: Option<&'static str>,
) -> std::io::Result<()> {
    for &byte in bytes {
        if byte == b'\r' {
            output.write_all(b"\r")?;
            state.line_start = true;
            continue;
        }
        let line_start = state.line_start;
        let channel_switch =
            channel_idx.is_some() && state.last_channel != channel_idx && state.channel_labels;
        if state.timestamps && line_start {
            let elapsed = timestamp.saturating_duration_since(state.started);
            let wall_timestamp = state.started_wall
                + chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());
            write!(
                output,
                "[{}] ",
                wall_timestamp.format("%Y-%m-%d %H:%M:%S%.3f")
            )?;
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

        if byte == b'\n' {
            if state.interactive {
                output.write_all(b"\r")?;
            }
            state.line_start = true;
        } else {
            state.line_start = false;
        }
        output.write_all(&[byte])?;
        if byte == b'\n' && line_color.is_some() {
            output.write_all(b"\x1b[0m")?;
        }
    }

    if line_color.is_some() && !state.line_start {
        output.write_all(b"\x1b[0m")?;
    }

    Ok(())
}

fn render_terminal_chunk(
    channel: u32,
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let (complete, partial) = {
        let stream = state.streams.entry(channel).or_insert_with(|| TextStream {
            bytes: Vec::new(),
            pending_cr: false,
        });
        let mut complete = Vec::new();
        for &byte in bytes {
            if stream.pending_cr {
                stream.pending_cr = false;
                if byte == b'\n' {
                    complete.push(std::mem::take(&mut stream.bytes));
                    continue;
                }
                stream.bytes.clear();
            }
            match byte {
                b'\r' => stream.pending_cr = true,
                b'\n' => complete.push(std::mem::take(&mut stream.bytes)),
                byte => stream.bytes.push(byte),
            }
        }
        (complete, stream.bytes.clone())
    };

    for line in complete {
        let foreground = erase_foreground(state, output)?;
        render_channel_bytes(&line, Some(channel), timestamp, state, output)?;
        if state.interactive {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
        if let Some(saved) = foreground {
            if saved.channel != channel {
                render_channel_bytes(&saved.bytes, Some(saved.channel), timestamp, state, output)?;
                state.foreground = Some(saved);
            }
        }
    }

    if !partial.is_empty() && state.interactive {
        if state.foreground.as_ref().map(|line| line.channel) == Some(channel) {
            erase_foreground(state, output)?;
        } else if state.foreground.is_some() {
            return Ok(());
        }
        render_channel_bytes(&partial, Some(channel), timestamp, state, output)?;
        state.foreground = Some(ForegroundLine {
            channel,
            bytes: partial,
        });
    }
    Ok(())
}

fn render_complete_line(
    channel: u32,
    bytes: &[u8],
    timestamp: Instant,
    color: Option<&'static str>,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let foreground = erase_foreground(state, output)?;
    render_channel_bytes_colored(bytes, Some(channel), timestamp, state, output, color)?;
    if !state.line_start {
        if state.interactive {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
    }
    if let Some(saved) = foreground {
        render_channel_bytes(&saved.bytes, Some(saved.channel), timestamp, state, output)?;
        state.foreground = Some(saved);
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
                        .write_terminal(event.channel_idx, bytes, state.channel_labels)
                        .map_err(io_error)?;
                }
                render_terminal_chunk(event.channel_idx, bytes, event.timestamp, state, output)?
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
                        .write_text(event.channel_idx, line.as_bytes(), state.channel_labels)
                        .map_err(io_error)?;
                }
                let level_color = if state.color {
                    let color = defmt_level_color(frame.level);
                    (!color.is_empty()).then_some(color)
                } else {
                    None
                };
                render_complete_line(
                    event.channel_idx,
                    line.as_bytes(),
                    event.timestamp,
                    level_color,
                    state,
                    output,
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
                        .write_text(event.channel_idx, line.as_bytes(), state.channel_labels)
                        .map_err(io_error)?;
                }
                render_complete_line(
                    event.channel_idx,
                    line.as_bytes(),
                    event.timestamp,
                    None,
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
        "\r\nCtrl-T commands:\r\n  q  Quit\r\n  ?  Show this help\r\n  c  Show configuration\r\n  l  Clear screen\r\n  t  Toggle timestamps\r\n  e  Toggle local echo\r\n  R  Reset target\r\n  Ctrl-T  Send a literal Ctrl-T\r\n\r\nCtrl-C is sent to the target.\r\n\r\nNot implemented from tio (not applicable to RTT):\r\n  serial port settings, device auto-connect/reconnect, input/output hex modes,\r\n  output delays, character mapping, scripts, socket/exec redirection, RS-485,\r\n  connect alerts, and tio-specific log file options.\r\n"
    )?;
    output.flush()
}

fn write_session_banner(output: &mut impl Write) -> std::io::Result<()> {
    write!(
        output,
        "brtt {}\r\nPress ctrl-t ? for help\r\nConnected to target\r\n",
        env!("CARGO_PKG_VERSION")
    )?;
    output.flush()
}

fn write_config(
    config: &SessionConfig,
    state: &SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    write!(output, "\r\nConfiguration:\r\n")?;
    write!(output, "  Probe: {}\r\n", config.probe)?;
    write!(output, "  Chip: {}\r\n", config.chip)?;
    write!(output, "  Up channels:")?;
    for spec in &config.up_specs {
        write!(output, " {}:{}", spec.index, spec.mode.name())?;
    }
    write!(output, "\r\n")?;
    if config.down_configured {
        write!(output, "  Down channel: {}\r\n", config.down_channel)?;
    } else {
        write!(output, "  Down channel: disabled\r\n")?;
    }
    write!(
        output,
        "  Poll interval: {} ms\r\n",
        config.poll_interval.as_millis()
    )?;
    write!(output, "  Timestamps: {}\r\n", on_or_off(state.timestamps))?;
    write!(output, "  Local echo: {}\r\n", on_or_off(state.local_echo))?;
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

fn write_toggle_status(output: &mut impl Write, label: &str, enabled: bool) -> std::io::Result<()> {
    write!(output, "\r\n{label}: {}\r\n", on_or_off(enabled))
}

fn interactive_input_available(has_down_channel: bool) -> bool {
    has_down_channel && std::io::stdin().is_terminal()
}

fn dispatch_command<'table>(
    command: SessionCommand,
    core: &mut Core,
    rtt: &mut Rtt,
    config: &'table SessionConfig,
    up_readers: &mut [UpChannelReader<'table>],
    state: &mut SessionState,
    output: &mut impl Write,
) -> Result<bool> {
    if command == SessionCommand::Quit {
        return Ok(true);
    }
    let suspended = erase_foreground(state, output)?;
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
            write_toggle_status(output, "Timestamps", state.timestamps)?;
            output.flush()?;
            state.line_start = true;
        }
        SessionCommand::ToggleLocalEcho => {
            state.local_echo = !state.local_echo;
            write_toggle_status(output, "Local echo", state.local_echo)?;
            output.flush()?;
            state.line_start = true;
        }
        SessionCommand::ResetTarget => {
            reset_and_reattach(core, rtt, &config.scan_region, config.automatic_scan)?;
            validate_channels(rtt, config)?;
            for reader in up_readers {
                reader.reset(config.defmt.as_ref())?;
            }
            write!(output, "\r\nTarget reset.\r\n")?;
            output.flush()?;
            state.line_start = true;
        }
    }

    if let Some(saved) = suspended {
        state.line_start = true;
        state.last_channel = None;
        render_channel_bytes_colored_inner(
            &saved.bytes,
            Some(saved.channel),
            Instant::now(),
            state,
            output,
            None,
        )?;
        state.foreground = Some(saved);
    }

    Ok(false)
}

pub(crate) struct SessionConfig {
    pub(crate) probe: String,
    pub(crate) chip: String,
    pub(crate) up_specs: Vec<ChannelSpec>,
    pub(crate) down_channel: usize,
    pub(crate) down_configured: bool,
    pub(crate) poll_interval: Duration,
    pub(crate) reset: bool,
    pub(crate) timestamps: bool,
    pub(crate) defmt: Option<DefmtData>,
    pub(crate) defmt_filters: Option<Vec<(String, defmt_parser::Level)>>,
    pub(crate) color: crate::cli::ColorMode,
    pub(crate) log: Option<std::path::PathBuf>,
    pub(crate) log_per_channel: bool,
    pub(crate) log_format: crate::cli::LogFormat,
    pub(crate) scan_region: ScanRegion,
    pub(crate) automatic_scan: bool,
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

pub(crate) fn run_session(core: &mut Core, mut rtt: Rtt, config: SessionConfig) -> Result<()> {
    if config.reset {
        reset_and_reattach(core, &mut rtt, &config.scan_region, config.automatic_scan)?;
    }

    validate_channels(&mut rtt, &config)?;
    let defmt_ref = config.defmt.as_ref();
    let mut up_readers = config
        .up_specs
        .iter()
        .copied()
        .map(|spec| UpChannelReader::new(spec, defmt_ref))
        .collect::<Result<Vec<_>>>()?;

    let mut logger = Logger::new(
        config.log.as_deref(),
        config.log_per_channel,
        config.log_format,
        config.up_specs.len(),
    )?;

    let mut down_buf = Vec::new();
    let mut escape_state = EscapeState::Normal;
    let mut state = SessionState::new();
    state.timestamps = config.timestamps;
    state.interactive = std::io::stdout().is_terminal();
    state.channel_labels = config.up_specs.len() > 1;
    state.color = match config.color {
        crate::cli::ColorMode::Always => true,
        crate::cli::ColorMode::Never => false,
        crate::cli::ColorMode::Auto => {
            std::io::IsTerminal::is_terminal(&std::io::stdout())
                && std::env::var_os("NO_COLOR").is_none()
        }
    };
    let stdin_setup = config.down_configured
        && interactive_input_available(
            channel_by_number(rtt.down_channels(), config.down_channel).is_some(),
        );
    if stdin_setup {
        // Ask the target shell to redraw its normal prompt at session start.
        down_buf.push(b'\n');
    }

    let _raw_mode = if stdin_setup {
        terminal::enable_raw_mode()?;
        Some(RawModeGuard)
    } else {
        None
    };

    let mut output = stdout();
    if state.interactive {
        write_session_banner(&mut output)?;
    }
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

        if stdin_setup {
            let input_ready = match event::poll(config.poll_interval) {
                Ok(ready) => ready,
                Err(err) => break 'read_loop Err(err.into()),
            };
            if input_ready {
                let event = match event::read() {
                    Ok(event) => event,
                    Err(err) => break 'read_loop Err(err.into()),
                };
                if let Event::Key(key_event) = event {
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
                                &mut up_readers,
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
            }
        } else {
            std::thread::sleep(config.poll_interval);
        }

        if config.down_configured {
            if let Some(down_channel) = channel_by_number(rtt.down_channels(), config.down_channel)
            {
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

    struct TestChannel(usize);

    impl RttChannel for TestChannel {
        fn number(&self) -> usize {
            self.0
        }

        fn name(&self) -> Option<&str> {
            None
        }

        fn buffer_size(&self) -> usize {
            0
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn channel_lookup_uses_rtt_number_not_slice_index() {
        let mut channels = [TestChannel(1), TestChannel(3)];

        assert_eq!(channel_by_number(&mut channels, 1).unwrap().number(), 1);
        assert_eq!(channel_by_number(&mut channels, 3).unwrap().number(), 3);
        assert!(channel_by_number(&mut channels, 0).is_none());
        assert!(channel_by_number(&mut channels, 2).is_none());
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
    fn ctrl_c_is_forwarded_to_the_target() {
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            (EscapeState::Normal, InputAction::Send(vec![0x03]))
        );
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
    fn control_and_alt_keys_keep_terminal_encoding() {
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            (EscapeState::Normal, InputAction::Send(vec![1]))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            (EscapeState::Normal, InputAction::Send(vec![21]))
        );
        assert_eq!(
            EscapeState::Normal.handle_key(key(KeyCode::Char('b'), KeyModifiers::ALT)),
            (EscapeState::Normal, InputAction::Send(b"\x1bb".to_vec()))
        );
    }

    #[test]
    fn repeated_keys_are_forwarded() {
        let mut repeated = key(KeyCode::Char('x'), KeyModifiers::NONE);
        repeated.kind = KeyEventKind::Repeat;

        assert_eq!(
            EscapeState::Normal.handle_key(repeated),
            (EscapeState::Normal, InputAction::Send(b"x".to_vec()))
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
        assert!(output.contains("Ctrl-C is sent to the target"));
        assert!(output.contains("Not implemented from tio"));
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
        let expected =
            format!("[{expected_timestamp}] partial line\r\n[{expected_timestamp}] next");
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
        state.interactive = false;
        let mut output = Vec::new();
        let timestamp = Instant::now();

        render_events(
            &[ChannelEvent {
                channel_idx: 0,
                payload: ChannelPayload::Bytes(b"partial ".to_vec()),
                timestamp,
            }],
            &mut state,
            None,
            None,
            &mut output,
        )
        .unwrap();
        assert!(output.is_empty());

        render_events(
            &[ChannelEvent {
                channel_idx: 0,
                payload: ChannelPayload::Bytes(b"line\n".to_vec()),
                timestamp,
            }],
            &mut state,
            None,
            None,
            &mut output,
        )
        .unwrap();

        assert_eq!(output, b"partial line\n");
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
            0,
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

        render_terminal_chunk(0, b"> ", Instant::now(), &mut state, &mut output).unwrap();
        render_terminal_chunk(0, b"help\r\n", Instant::now(), &mut state, &mut output).unwrap();

        assert_eq!(output, b"[ch0] > \r\x1b[2K[ch0] > help\r\n");
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
            down_channel: 1,
            down_configured: true,
            poll_interval: Duration::from_millis(10),
            reset: false,
            timestamps: false,
            defmt: None,
            defmt_filters: None,
            color: crate::cli::ColorMode::Never,
            log: None,
            log_per_channel: false,
            log_format: crate::cli::LogFormat::Decoded,
            scan_region: ScanRegion::Exact(0x2000_0000),
            automatic_scan: false,
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
    fn channel_events_are_rendered_with_channel_labels_in_event_order() {
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
        state.channel_labels = true;

        render_events(&events, &mut state, None, None, &mut output).unwrap();

        assert_eq!(output, b"[ch2] log\r\n[ch0] shell");
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

    #[test]
    fn filtered_defmt_frames_are_not_rendered_or_logged() {
        let events = vec![ChannelEvent {
            channel_idx: 0,
            payload: ChannelPayload::Defmt(DecodedFrame {
                message: "quiet".to_string(),
                timestamp: None,
                level: Some(defmt_parser::Level::Info),
                module: Some("app".to_string()),
            }),
            timestamp: Instant::now(),
        }];
        let filters = vec![(String::new(), defmt_parser::Level::Warn)];
        let mut output = Vec::new();
        let mut state = SessionState::new();

        render_events(&events, &mut state, Some(&filters), None, &mut output).unwrap();

        assert!(output.is_empty());
        assert_eq!(state.defmt_decode_warnings, 0);
    }
}
