use crate::channel::ChannelId;
use crate::cli::{ColorMode, SessionConfig};
use crate::defmt::{filter_level, level_enabled, level_name, DecodedFrame, Filter};
use crate::logger::Logger;
use crate::terminal::{PartialView, TerminalChunk, ANSI_RESET, ERASE_CURRENT_LINE};
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::time::Instant;

/// Per-lifecycle rendering state shared by terminal and defmt output.
struct SessionState {
    timestamps: bool,
    line_start: bool,
    started: Instant,
    started_wall: DateTime<Local>,
    defmt_decode_warnings: u64,
    color: bool,
    channel_labels: bool,
    last_channel: Option<ChannelId>,
    /// Cached tails for redirected output.
    partials: HashMap<ChannelId, Vec<u8>>,
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

impl SessionState {
    fn new() -> Self {
        Self {
            timestamps: false,
            line_start: true,
            started: Instant::now(),
            started_wall: Local::now(),
            defmt_decode_warnings: 0,
            color: false,
            channel_labels: false,
            last_channel: None,
            partials: HashMap::new(),
            presentation: Presentation::Interactive { foreground: None },
        }
    }

    fn reset_target(&mut self) {
        self.line_start = true;
        self.last_channel = None;
        self.partials.clear();
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

    fn foreground_is(&self, channel: ChannelId) -> bool {
        self.foreground().map(|line| line.channel) == Some(channel)
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

pub(crate) struct Renderer<W: Write> {
    state: SessionState,
    logger: Option<Logger>,
    filters: Option<Box<[Filter]>>,
    output: W,
}

impl<W: Write> Renderer<W> {
    fn new(
        output: W,
        logger: Option<Logger>,
        filters: Option<Box<[Filter]>>,
        state: SessionState,
    ) -> Self {
        Self {
            state,
            logger,
            filters,
            output,
        }
    }

    pub(crate) fn for_session(output: W, logger: Option<Logger>, config: &SessionConfig) -> Self {
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
        Self::new(
            output,
            logger,
            config.defmt_filters.as_deref().map(Into::into),
            state,
        )
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

    pub(crate) fn is_interactive(&self) -> bool {
        self.state.is_interactive()
    }

    fn finish_redirected_partials(&mut self) -> std::io::Result<()> {
        debug_assert!(!self.state.is_interactive());

        let mut partials: Vec<_> = self
            .state
            .partials
            .iter()
            .filter(|(_, display)| !display.is_empty())
            .map(|(&channel, display)| (channel, display.clone()))
            .collect();
        partials.sort_unstable_by_key(|(channel, _)| *channel);
        for (channel, bytes) in partials {
            render_channel_bytes(
                &bytes,
                channel,
                Instant::now(),
                &mut self.state,
                &mut self.output,
                None,
            )?;
            self.output.write_all(b"\n")?;
        }
        self.state.line_start = true;
        Ok(())
    }

    pub(crate) fn render_terminal_event(
        &mut self,
        channel: ChannelId,
        chunk: TerminalChunk,
        timestamp: Instant,
    ) -> std::io::Result<()> {
        render_terminal_event(
            channel,
            chunk,
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
            self.filters.as_deref(),
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
            self.output.write_all(ANSI_RESET)?;
            self.output.write_all(ERASE_CURRENT_LINE)?;
            self.output.write_all(b"\r\n")?;
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

    /// Takes the foreground line so a command can write, caller restores it.
    pub(crate) fn suspend_foreground(&mut self) -> std::io::Result<Option<ForegroundLine>> {
        erase_foreground(&mut self.state, &mut self.output)
    }

    pub(crate) fn restore_foreground(&mut self, saved: ForegroundLine) -> std::io::Result<()> {
        self.state.line_start = true;
        self.state.last_channel = None;
        render_channel_bytes(
            &saved.bytes,
            saved.channel,
            Instant::now(),
            &mut self.state,
            &mut self.output,
            None,
        )?;
        self.state.set_foreground(saved);
        Ok(())
    }
}

fn contains_sgr(bytes: &[u8]) -> bool {
    let mut escape = false;
    let mut csi = false;
    for &byte in bytes {
        if escape {
            csi = byte == b'[';
            escape = false;
        } else if csi {
            if byte == b'm' {
                return true;
            }
            if (0x40..=0x7e).contains(&byte) {
                csi = false;
            }
        } else if byte == 0x1b {
            escape = true;
        }
    }
    false
}

fn ends_with_sgr_reset(bytes: &[u8]) -> bool {
    bytes.ends_with(b"\x1b[0m") || bytes.ends_with(b"\x1b[m")
}
fn erase_foreground(
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<Option<ForegroundLine>> {
    let foreground = state.take_foreground();
    if foreground
        .as_ref()
        .is_some_and(|line| contains_sgr(&line.bytes))
        && state.is_interactive()
    {
        // The saved foreground may leave SGR attributes active (shell
        // prompts commonly do). Reset them before rendering unrelated
        // output, otherwise asynchronous MCU logs inherit the prompt color.
        output.write_all(ERASE_CURRENT_LINE)?;
        output.write_all(ANSI_RESET)?;
    } else if foreground.is_some() && state.is_interactive() {
        output.write_all(ERASE_CURRENT_LINE)?;
    }
    state.line_start = true;
    state.last_channel = None;
    Ok(foreground)
}

fn render_channel_bytes(
    bytes: &[u8],
    channel: ChannelId,
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
        let channel_switch = state.last_channel != Some(channel) && state.channel_labels;
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
            if state.color {
                write!(output, "{}", channel_color(channel))?;
            }
            write!(output, "[ch{channel}] ")?;
            if state.color {
                output.write_all(ANSI_RESET)?;
            }
            state.last_channel = Some(channel);
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
            output.write_all(ANSI_RESET)?;
        }
    }

    if line_color.is_some() && !state.line_start {
        output.write_all(ANSI_RESET)?;
    }

    Ok(())
}

fn render_terminal_chunk(
    channel: ChannelId,
    chunk: TerminalChunk,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let TerminalChunk { lines, partial } = chunk;
    let PartialView {
        log: _,
        display,
        overlay,
    } = partial;
    // Only redirected mode reads this cache (`finish_redirected_partials`);
    // interactive mode tracks the visible tail in `foreground` instead.
    for line in &lines {
        let reset_after_line = state.is_interactive()
            && contains_sgr(&line.display)
            && !ends_with_sgr_reset(&line.display);
        let foreground = erase_foreground(state, output)?;
        render_channel_bytes(&line.display, channel, timestamp, state, output, None)?;
        if state.is_interactive() {
            if reset_after_line {
                // A raw terminal line may open an SGR attribute without
                // closing it. Isolate the following logical line.
                output.write_all(ANSI_RESET)?;
            }
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
        if let Some(saved) = foreground {
            if saved.channel != channel {
                render_channel_bytes(&saved.bytes, saved.channel, timestamp, state, output, None)?;
                state.set_foreground(saved);
            }
        }
    }

    if !state.is_interactive() {
        state.partials.insert(channel, display);
        return Ok(());
    }
    if display.is_empty() {
        if state.foreground_is(channel) {
            erase_foreground(state, output)?;
        }
        return Ok(());
    }
    if state.foreground_is(channel) {
        erase_foreground(state, output)?;
    } else if state.foreground().is_some() {
        return Ok(());
    }
    render_channel_bytes(&overlay, channel, timestamp, state, output, None)?;
    state.set_foreground(ForegroundLine {
        channel,
        bytes: overlay,
    });
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
    render_channel_bytes(bytes, channel, timestamp, state, output, color)?;
    if !state.line_start {
        if state.is_interactive() {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
    }
    if let Some(saved) = foreground {
        render_channel_bytes(&saved.bytes, saved.channel, timestamp, state, output, None)?;
        state.set_foreground(saved);
    }
    Ok(())
}

fn channel_color(channel: ChannelId) -> &'static str {
    [
        "\x1b[36m", "\x1b[35m", "\x1b[34m", "\x1b[32m", "\x1b[33m", "\x1b[31m",
    ][channel.value() % 6]
}

fn render_terminal_event(
    channel: ChannelId,
    chunk: TerminalChunk,
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> std::io::Result<()> {
    if let Some(logger) = logger {
        logger
            .write_terminal_decoded(
                channel.value(),
                chunk.lines.iter().map(|line| line.log.as_slice()),
                &chunk.partial.log,
            )
            .map_err(io_error)?;
    }
    render_terminal_chunk(channel, chunk, timestamp, state, output)
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
        "\r\nCtrl-T commands:\r\n  q  Quit\r\n  ?  Show this help\r\n  c  Show configuration\r\n  l  Clear screen\r\n  t  Toggle timestamps\r\n  R  Reset target\r\n  Ctrl-T  Send a literal Ctrl-T\r\n\r\nCtrl-C is sent to the target.\r\n\r\n"
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
