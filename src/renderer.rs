use crate::channel::ChannelId;
use crate::cli::{ColorMode, SessionConfig};
use crate::defmt::{filter_level, level_enabled, level_name, DecodedFrame, Filter};
use crate::logger::Logger;
use crate::terminal::DecodedStream;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::Instant;

const MAX_SESSION_RAW_LINE_BYTES: usize = 4096;
const MAX_RAW_ESCAPE_BYTES: usize = 32;

/// Per-lifecycle rendering state shared by terminal and defmt output.
struct SessionState {
    timestamps: bool,
    local_echo: bool,
    line_start: bool,
    started: Instant,
    started_wall: DateTime<Local>,
    defmt_decode_warnings: u64,
    color: bool,
    channel_labels: bool,
    last_channel: Option<ChannelId>,
    streams: HashMap<ChannelId, SessionStream>,
    presentation: Presentation,
}

pub(crate) struct ForegroundLine {
    channel: ChannelId,
    bytes: Vec<u8>,
}

enum Presentation {
    Interactive { foreground: Option<ForegroundLine> },
    Redirected,
}

enum RawInputState {
    Text,
    PendingCr,
    Escape(Vec<u8>),
}

struct CompletedRawLine {
    bytes: Vec<u8>,
    requires_terminal_rendering: bool,
}

struct SessionStreamOutput {
    complete: Vec<Vec<u8>>,
    partial: Vec<u8>,
}

/// One up channel's incremental terminal decoding state.
struct SessionStream {
    terminal: DecodedStream,
    raw_line: Vec<u8>,
    raw_state: RawInputState,
    requires_terminal_rendering: bool,
    completed_raw_lines: Vec<CompletedRawLine>,
}

impl SessionStream {
    fn new() -> Self {
        Self {
            terminal: DecodedStream::new(),
            raw_line: Vec::new(),
            raw_state: RawInputState::Text,
            requires_terminal_rendering: false,
            completed_raw_lines: Vec::new(),
        }
    }

    fn consume(&mut self, bytes: &[u8], styled: bool) -> SessionStreamOutput {
        let terminal_complete = self.terminal.consume_styled(bytes);
        self.consume_raw(bytes);
        let raw_complete = std::mem::take(&mut self.completed_raw_lines);
        let complete = terminal_complete
            .into_iter()
            .enumerate()
            .map(|(index, (mut plain, styled_line))| {
                if plain.last() == Some(&b'\n') {
                    plain.pop();
                }
                match raw_complete.get(index) {
                    Some(CompletedRawLine {
                        bytes,
                        requires_terminal_rendering: false,
                    }) => bytes.clone(),
                    _ if styled => styled_line,
                    _ => plain,
                }
            })
            .collect();
        SessionStreamOutput {
            complete,
            partial: self.visible_line(),
        }
    }

    fn rendered_visible_line(&self) -> Vec<u8> {
        let mut line = if self.requires_terminal_rendering {
            self.terminal.styled_visible_line()
        } else {
            self.raw_line.clone()
        };
        if self.requires_terminal_rendering {
            let cursor = self.terminal.cursor_column();
            let mut positioned = b"\x1b7".to_vec();
            positioned.append(&mut line);
            positioned.extend_from_slice(b"\x1b8");
            if cursor > 0 {
                positioned.extend_from_slice(format!("\x1b[{cursor}C").as_bytes());
            }
            positioned.extend_from_slice(&self.terminal.active_attributes());
            line = positioned;
        }
        line
    }

    fn visible_line(&self) -> Vec<u8> {
        if self.requires_terminal_rendering {
            self.terminal.visible_line()
        } else {
            self.raw_line.clone()
        }
    }

    fn consume_raw(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if matches!(self.raw_state, RawInputState::PendingCr) {
                self.raw_state = RawInputState::Text;
                if byte == b'\n' {
                    self.finish_raw_line();
                    continue;
                }
                self.requires_terminal_rendering = true;
            }

            match byte {
                b'\r' => self.raw_state = RawInputState::PendingCr,
                b'\n' => self.finish_raw_line(),
                b'\x1b' => {
                    self.raw_state = RawInputState::Escape(vec![byte]);
                    self.push_raw(byte);
                }
                byte => {
                    self.push_raw(byte);
                    self.update_escape(byte);
                }
            }
        }
    }

    fn update_escape(&mut self, byte: u8) {
        let RawInputState::Escape(escape) = &mut self.raw_state else {
            return;
        };

        if escape.len() < MAX_RAW_ESCAPE_BYTES {
            escape.push(byte);
        } else {
            self.requires_terminal_rendering = true;
        }

        if escape.len() == 2 && byte != b'[' {
            self.requires_terminal_rendering = true;
            self.raw_state = RawInputState::Text;
        } else if escape.len() >= 3 && (0x40..=0x7e).contains(&byte) {
            if escape.get(1) != Some(&b'[') || byte != b'm' {
                self.requires_terminal_rendering = true;
            }
            self.raw_state = RawInputState::Text;
        }
    }

    fn finish_raw_line(&mut self) {
        self.completed_raw_lines.push(CompletedRawLine {
            bytes: std::mem::take(&mut self.raw_line),
            requires_terminal_rendering: self.requires_terminal_rendering,
        });
        self.raw_state = RawInputState::Text;
        self.requires_terminal_rendering = false;
    }

    fn push_raw(&mut self, byte: u8) {
        if self.raw_line.len() < MAX_SESSION_RAW_LINE_BYTES {
            self.raw_line.push(byte);
        } else {
            self.requires_terminal_rendering = true;
        }
    }
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
            presentation: Presentation::Interactive { foreground: None },
        }
    }

    /// Clears all per-target rendering state after a target restart.
    fn reset_target(&mut self) {
        self.line_start = true;
        self.last_channel = None;
        self.streams.clear();
        if let Presentation::Interactive { foreground } = &mut self.presentation {
            *foreground = None;
        }
    }

    fn is_interactive(&self) -> bool {
        matches!(self.presentation, Presentation::Interactive { .. })
    }

    fn foreground(&self) -> Option<&ForegroundLine> {
        match &self.presentation {
            Presentation::Interactive { foreground } => foreground.as_ref(),
            Presentation::Redirected => None,
        }
    }

    fn take_foreground(&mut self) -> Option<ForegroundLine> {
        match &mut self.presentation {
            Presentation::Interactive { foreground } => foreground.take(),
            Presentation::Redirected => None,
        }
    }

    fn set_foreground(&mut self, line: ForegroundLine) {
        let Presentation::Interactive { foreground } = &mut self.presentation else {
            unreachable!("redirected presentation cannot contain a foreground line")
        };
        *foreground = Some(line);
    }
}

/// Owns all host-side output and the state needed to present it consistently.
pub(crate) struct Renderer<'filters, W: Write> {
    state: SessionState,
    logger: Option<Logger>,
    filters: Option<&'filters [Filter]>,
    output: W,
}

impl<'filters, W: Write> Renderer<'filters, W> {
    fn new(
        output: W,
        logger: Option<Logger>,
        filters: Option<&'filters [Filter]>,
        state: SessionState,
    ) -> Self {
        Self {
            state,
            logger,
            filters,
            output,
        }
    }

    /// Builds a renderer configured from a validated session configuration.
    pub(crate) fn for_session(
        output: W,
        logger: Option<Logger>,
        filters: Option<&'filters [Filter]>,
        config: &SessionConfig,
    ) -> Self {
        let mut state = SessionState::new();
        state.timestamps = config.timestamps;
        state.presentation = if std::io::stdout().is_terminal() {
            Presentation::Interactive { foreground: None }
        } else {
            Presentation::Redirected
        };
        state.channel_labels = config.up_specs.len() > 1;
        state.color = match config.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
            }
        };
        Self::new(output, logger, filters, state)
    }

    pub(crate) fn finish_target_epoch(&mut self) -> Result<()> {
        if self.state.is_interactive() {
            erase_foreground(&mut self.state, &mut self.output)?;
        } else {
            self.finish_redirected_partials()?;
        }
        if let Some(logger) = &mut self.logger {
            logger.reset()?;
        }
        Ok(())
    }

    pub(crate) fn reset_target_epoch(&mut self) -> Result<()> {
        self.finish_target_epoch()?;
        self.state.reset_target();
        Ok(())
    }

    fn finish_redirected_partials(&mut self) -> std::io::Result<()> {
        debug_assert!(!self.state.is_interactive());

        let mut partials: Vec<_> = self
            .state
            .streams
            .iter()
            .filter_map(|(&channel, stream)| {
                let bytes = stream.visible_line();
                if bytes.is_empty() {
                    None
                } else {
                    Some((channel, bytes))
                }
            })
            .collect();
        partials.sort_unstable_by_key(|(channel, _)| *channel);
        for (channel, bytes) in partials {
            render_channel_bytes(
                &bytes,
                Some(channel),
                Instant::now(),
                &mut self.state,
                &mut self.output,
            )?;
            self.output.write_all(b"\n")?;
            self.state.line_start = true;
        }
        Ok(())
    }

    pub(crate) fn render_terminal_event(
        &mut self,
        channel: ChannelId,
        bytes: &[u8],
        timestamp: Instant,
    ) -> std::io::Result<()> {
        render_terminal_event(
            channel,
            bytes,
            timestamp,
            &mut self.state,
            self.logger.as_mut(),
            &mut self.output,
        )
    }

    pub(crate) fn render_defmt_frame(
        &mut self,
        channel: ChannelId,
        frame: &DecodedFrame<'_>,
        timestamp: Instant,
    ) -> std::io::Result<()> {
        render_defmt_frame(
            channel,
            frame,
            timestamp,
            self.filters,
            &mut self.state,
            self.logger.as_mut(),
            &mut self.output,
        )
    }

    pub(crate) fn render_defmt_warning(
        &mut self,
        channel: ChannelId,
        warning: &str,
        timestamp: Instant,
    ) -> std::io::Result<()> {
        render_defmt_warning(
            channel,
            warning,
            timestamp,
            &mut self.state,
            self.logger.as_mut(),
            &mut self.output,
        )
    }

    /// Records raw RTT bytes in a raw log, when one is configured.
    pub(crate) fn log_raw_bytes(&mut self, channel: ChannelId, bytes: &[u8]) -> Result<()> {
        if let Some(logger) = self.logger.as_mut() {
            logger.write_bytes(channel.value(), bytes)?;
        }
        Ok(())
    }

    pub(crate) fn flush_data(&mut self) -> Result<()> {
        if let Some(logger) = &mut self.logger {
            logger.flush_files()?;
        }
        self.output.flush().context("Error writing to stdout")
    }

    pub(crate) fn flush_output(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }

    pub(crate) fn finish_session(&mut self) -> Result<()> {
        if self.state.is_interactive() {
            self.output.write_all(b"\x1b[0m\r\x1b[2K\r\n")?;
        } else {
            self.finish_redirected_partials()?;
        }
        self.output.flush().context("Error writing to stdout")?;
        if let Some(logger) = &mut self.logger {
            logger.flush()?;
        }
        Ok(())
    }

    // -- Ctrl-T command presentation -------------------------------------

    pub(crate) fn show_banner(&mut self) -> std::io::Result<()> {
        if self.state.is_interactive() {
            write_session_banner(&mut self.output)?;
        }
        Ok(())
    }

    pub(crate) fn show_help(&mut self) -> std::io::Result<()> {
        write_help(&mut self.output)?;
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn show_config(&mut self, config: &SessionConfig) -> std::io::Result<()> {
        write_config(config, &self.state, &mut self.output)?;
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn clear_screen(&mut self) -> std::io::Result<()> {
        clear_screen(&mut self.output)?;
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn toggle_timestamps(&mut self) -> std::io::Result<()> {
        self.state.timestamps = !self.state.timestamps;
        write_toggle_status(&mut self.output, "Timestamps", self.state.timestamps)?;
        self.output.flush()?;
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn toggle_local_echo(&mut self) -> std::io::Result<()> {
        self.state.local_echo = !self.state.local_echo;
        write_toggle_status(&mut self.output, "Local echo", self.state.local_echo)?;
        self.output.flush()?;
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn notice_target_reset(&mut self) -> std::io::Result<()> {
        write!(self.output, "\r\nTarget reset.\r\n")?;
        self.output.flush()?;
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn notice_reattached(&mut self) -> std::io::Result<()> {
        if self.state.is_interactive() {
            self.output
                .write_all(b"\r\nRTT control block changed; reattached to target.\r\n")?;
            self.output.flush()?;
        }
        Ok(())
    }

    /// Removes the foreground partial line before a command writes to the
    /// terminal, returning it so the caller can restore it afterwards.
    pub(crate) fn suspend_foreground(&mut self) -> std::io::Result<Option<ForegroundLine>> {
        erase_foreground(&mut self.state, &mut self.output)
    }

    pub(crate) fn restore_foreground(&mut self, saved: ForegroundLine) -> std::io::Result<()> {
        self.state.line_start = true;
        self.state.last_channel = None;
        render_channel_bytes_colored_inner(
            &saved.bytes,
            Some(saved.channel),
            Instant::now(),
            &mut self.state,
            &mut self.output,
            None,
        )?;
        self.state.set_foreground(saved);
        Ok(())
    }

    pub(crate) fn local_echo_enabled(&self) -> bool {
        self.state.local_echo
    }

    pub(crate) fn render_local_echo(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        render_bytes(bytes, Instant::now(), &mut self.state, &mut self.output)
    }
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
    channel_idx: Option<ChannelId>,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    render_channel_bytes_colored(bytes, channel_idx, timestamp, state, output, None)
}

fn render_channel_bytes_colored(
    bytes: &[u8],
    channel_idx: Option<ChannelId>,
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
    let foreground = state.take_foreground();
    if foreground.is_some() && state.is_interactive() {
        output.write_all(b"\r\x1b[2K")?;
    }
    state.line_start = true;
    state.last_channel = None;
    Ok(foreground)
}

fn render_channel_bytes_colored_inner(
    bytes: &[u8],
    channel_idx: Option<ChannelId>,
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
            if state.is_interactive() {
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
    channel: ChannelId,
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let (complete, partial, rendered_partial) = {
        let styled = state.is_interactive();
        let stream = state
            .streams
            .entry(channel)
            .or_insert_with(SessionStream::new);
        let SessionStreamOutput { complete, partial } = stream.consume(bytes, styled);
        (complete, partial, stream.rendered_visible_line())
    };

    for line in complete {
        let foreground = erase_foreground(state, output)?;
        render_channel_bytes(&line, Some(channel), timestamp, state, output)?;
        if state.is_interactive() {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
        if let Some(saved) = foreground {
            if saved.channel != channel {
                render_channel_bytes(&saved.bytes, Some(saved.channel), timestamp, state, output)?;
                state.set_foreground(saved);
            }
        }
    }

    if state.is_interactive() {
        if partial.is_empty() {
            if state.foreground().map(|line| line.channel) == Some(channel) {
                erase_foreground(state, output)?;
            }
        } else {
            if state.foreground().map(|line| line.channel) == Some(channel) {
                erase_foreground(state, output)?;
            } else if state.foreground().is_some() {
                return Ok(());
            }
            let partial = rendered_partial;
            render_channel_bytes(&partial, Some(channel), timestamp, state, output)?;
            state.set_foreground(ForegroundLine {
                channel,
                bytes: partial,
            });
        }
    }
    Ok(())
}

fn render_complete_line(
    channel: ChannelId,
    bytes: &[u8],
    timestamp: Instant,
    color: Option<&'static str>,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let foreground = erase_foreground(state, output)?;
    render_channel_bytes_colored(bytes, Some(channel), timestamp, state, output, color)?;
    if !state.line_start {
        if state.is_interactive() {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
    }
    if let Some(saved) = foreground {
        render_channel_bytes(&saved.bytes, Some(saved.channel), timestamp, state, output)?;
        state.set_foreground(saved);
    }
    Ok(())
}

fn channel_color(channel_idx: ChannelId) -> &'static str {
    [
        "\x1b[36m", "\x1b[35m", "\x1b[34m", "\x1b[32m", "\x1b[33m", "\x1b[31m",
    ][channel_idx.value() % 6]
}

fn render_terminal_event(
    channel: ChannelId,
    bytes: &[u8],
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    if let Some(logger) = logger {
        logger
            .write_chars(channel.value(), bytes)
            .map_err(io_error)?;
    }
    render_terminal_chunk(channel, bytes, timestamp, state, output)
}

fn render_defmt_frame(
    channel: ChannelId,
    frame: &DecodedFrame<'_>,
    timestamp: Instant,
    filters: Option<&[Filter]>,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    if let (Some(level), Some(filters)) = (frame.level, filters) {
        let minimum = filter_level(frame.module, filters);
        if !level_enabled(level, minimum) {
            return Ok(());
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
    if let Some(logger) = logger {
        logger
            .write_defmt_decoded(channel.value(), line.as_bytes())
            .map_err(io_error)?;
    }
    let level_color = if state.color {
        let color = defmt_level_color(frame.level);
        (!color.is_empty()).then_some(color)
    } else {
        None
    };
    render_complete_line(
        channel,
        line.as_bytes(),
        timestamp,
        level_color,
        state,
        output,
    )
}

fn render_defmt_warning(
    channel: ChannelId,
    warning: &str,
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    state.defmt_decode_warnings += 1;
    let line = format!(
        "[defmt warning #{}] {warning}\n",
        state.defmt_decode_warnings
    );
    if let Some(logger) = logger {
        logger
            .write_defmt_decoded(channel.value(), line.as_bytes())
            .map_err(io_error)?;
    }
    render_complete_line(channel, line.as_bytes(), timestamp, None, state, output)
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
        "\r\nCtrl-T commands:\r\n  q  Quit\r\n  ?  Show this help\r\n  c  Show configuration\r\n  l  Clear screen\r\n  t  Toggle timestamps\r\n  e  Toggle local echo\r\n  R  Reset target\r\n  Ctrl-T  Send a literal Ctrl-T\r\n\r\nCtrl-C is sent to the target.\r\n\r\n"
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
    if let Some(down_channel) = config.down_channel {
        write!(output, "  Down channel: {down_channel}\r\n")?;
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

#[cfg(test)]
#[path = "../tests/renderer.rs"]
mod tests;
