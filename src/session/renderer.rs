use super::core_io::UpSink;
use super::logger::Logger;
use super::terminal_decode::{TerminalChunk, ANSI_RESET, ERASE_CURRENT_LINE};
use super::{ColorMode, SessionPolicy};
use crate::channel::{ChannelId, CoreChannel, CoreId};
use crate::defmt::{level_name, DecodedFrame};
use crate::probe::ProbeIdentity;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use crossterm::{
    cursor, execute,
    terminal::{self, ClearType},
};
use std::collections::HashMap;
use std::io::{stdout, IsTerminal, StdoutLock, Write};
use std::time::Instant;

struct SessionState {
    timestamps: bool,
    line_start: bool,
    started: Instant,
    started_wall: DateTime<Local>,
    defmt_decode_warnings: u64,
    color: bool,
    channel_labels: bool,
    /// Whether channel tags include the core index (`[c0:ch0]` vs `[ch0]`).
    show_cores: bool,
    last_channel: Option<CoreChannel>,
    presentation: Presentation,
}

#[derive(Debug, Clone)]
pub(super) struct ForegroundLine {
    channel: CoreChannel,
    rendered: Vec<u8>,
}

struct CorePrompt {
    /// Ordered stack of channels with active prompts on this core (most recent last).
    channels: Vec<CoreChannel>,
    time: Option<Instant>,
}

struct InteractiveState {
    foreground: Option<CoreChannel>,
    prompts: HashMap<CoreChannel, ForegroundLine>,
    prompts_by_core: HashMap<CoreId, CorePrompt>,
    /// Routable down target; None lets any core show its prompt.
    selected_down_core: Option<CoreId>,
}

enum Presentation {
    Interactive(InteractiveState),
    Redirected {
        partials: HashMap<CoreChannel, Vec<u8>>,
    },
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
            show_cores: false,
            last_channel: None,
            presentation: Presentation::Interactive(InteractiveState {
                foreground: None,
                prompts: HashMap::new(),
                prompts_by_core: HashMap::new(),
                selected_down_core: None,
            }),
        }
    }

    fn prompt_time(&self, core: CoreId, fallback: Instant) -> Instant {
        match &self.presentation {
            Presentation::Interactive(state) => state
                .prompts_by_core
                .get(&core)
                .and_then(|prompt| prompt.time)
                .unwrap_or(fallback),
            Presentation::Redirected { .. } => fallback,
        }
    }

    fn selected_down_core(&self) -> Option<CoreId> {
        match &self.presentation {
            Presentation::Interactive(state) => state.selected_down_core,
            Presentation::Redirected { .. } => None,
        }
    }

    fn select_down_core(&mut self, core: Option<CoreId>) {
        if let Presentation::Interactive(state) = &mut self.presentation {
            state.selected_down_core = core;
        }
    }

    fn record_prompt_time(&mut self, core: CoreId, at: Instant) {
        if let Presentation::Interactive(state) = &mut self.presentation {
            state
                .prompts_by_core
                .entry(core)
                .or_insert_with(|| CorePrompt {
                    channels: Vec::new(),
                    time: None,
                })
                .time = Some(at);
        }
    }

    fn prompt_time_or_record(&mut self, core: CoreId, fallback: Instant) -> Instant {
        let Presentation::Interactive(state) = &mut self.presentation else {
            return fallback;
        };
        *state
            .prompts_by_core
            .entry(core)
            .or_insert_with(|| CorePrompt {
                channels: Vec::new(),
                time: None,
            })
            .time
            .get_or_insert(fallback)
    }

    fn reset_target(&mut self) {
        self.line_start = true;
        self.last_channel = None;
        match &mut self.presentation {
            Presentation::Interactive(state) => {
                state.foreground = None;
                state.prompts.clear();
                state.prompts_by_core.clear();
                state.selected_down_core = None;
            }
            Presentation::Redirected { partials } => partials.clear(),
        }
    }

    fn reset_core(&mut self, core: CoreId) {
        let foreground_is_reset_core = self
            .foreground()
            .is_some_and(|line| line.channel.core == core);
        if foreground_is_reset_core || self.foreground().is_none() {
            self.line_start = true;
        }
        if self.last_channel.is_some_and(|source| source.core == core) {
            self.last_channel = None;
        }
        match &mut self.presentation {
            Presentation::Interactive(state) => {
                state.prompts.retain(|source, _| source.core != core);
                state.prompts_by_core.remove(&core);
                if state.foreground.is_some_and(|source| source.core == core) {
                    state.foreground = None;
                }
            }
            Presentation::Redirected { partials } => {
                partials.retain(|source, _| source.core != core);
            }
        }
    }

    fn is_interactive(&self) -> bool {
        matches!(self.presentation, Presentation::Interactive(_))
    }

    fn foreground(&self) -> Option<&ForegroundLine> {
        match &self.presentation {
            Presentation::Interactive(state) => state
                .foreground
                .and_then(|channel| state.prompts.get(&channel)),
            Presentation::Redirected { .. } => None,
        }
    }

    fn foreground_is(&self, source: CoreChannel) -> bool {
        match &self.presentation {
            Presentation::Interactive(state) => state.foreground == Some(source),
            Presentation::Redirected { .. } => false,
        }
    }

    fn take_foreground(&mut self) -> Option<ForegroundLine> {
        let Presentation::Interactive(state) = &mut self.presentation else {
            return None;
        };
        let channel = state.foreground.take()?;
        state.prompts.get(&channel).cloned()
    }

    fn set_foreground(&mut self, line: ForegroundLine) {
        let channel = line.channel;
        self.cache_prompt(line);
        let Presentation::Interactive(state) = &mut self.presentation else {
            unreachable!("redirected presentation cannot contain a foreground line")
        };
        state.foreground = Some(channel);
    }

    fn cache_prompt(&mut self, line: ForegroundLine) {
        let channel = line.channel;
        let Presentation::Interactive(state) = &mut self.presentation else {
            unreachable!("redirected presentation cannot cache a prompt")
        };
        state.prompts.insert(channel, line);
        let entry = state
            .prompts_by_core
            .entry(channel.core)
            .or_insert_with(|| CorePrompt {
                channels: Vec::new(),
                time: None,
            });
        entry.channels.retain(|&c| c != channel);
        entry.channels.push(channel);
    }

    fn remove_prompt(&mut self, source: CoreChannel) {
        let Presentation::Interactive(state) = &mut self.presentation else {
            return;
        };
        state.prompts.remove(&source);
        if state.foreground == Some(source) {
            state.foreground = None;
        }
        if let Some(prompt) = state.prompts_by_core.get_mut(&source.core) {
            prompt.channels.retain(|&c| c != source);
            if prompt.channels.is_empty() && prompt.time.is_none() {
                state.prompts_by_core.remove(&source.core);
            }
        }
    }
}

pub(super) struct Renderer {
    state: SessionState,
    logger: Option<Logger>,
    output: StdoutLock<'static>,
}

impl Renderer {
    pub(super) fn for_session(
        config: &SessionPolicy,
        logger: Option<Logger>,
        include_channel: bool,
        show_cores: bool,
    ) -> Self {
        let is_terminal = stdout().is_terminal();
        let color_allowed = std::env::var_os("NO_COLOR").is_none();
        let mut state = SessionState::new();
        state.timestamps = config.timestamps;
        state.presentation = if is_terminal {
            Presentation::Interactive(InteractiveState {
                foreground: None,
                prompts: HashMap::new(),
                prompts_by_core: HashMap::new(),
                selected_down_core: None,
            })
        } else {
            Presentation::Redirected {
                partials: HashMap::new(),
            }
        };
        state.channel_labels = include_channel;
        state.show_cores = show_cores;
        state.color = match config.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => is_terminal && color_allowed,
        };
        Self {
            state,
            logger,
            output: stdout().lock(),
        }
    }

    fn finish_target_epoch(&mut self) -> Result<()> {
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

    pub(super) fn reset_target_epoch(&mut self) -> Result<()> {
        self.finish_target_epoch()?;
        self.state.reset_target();
        Ok(())
    }

    fn finish_core_epoch(&mut self, core: CoreId) -> Result<()> {
        if self.state.is_interactive() {
            self.erase_core_prompt(core)?;
        } else {
            self.finish_redirected_partials_for(core)?;
        }
        if let Some(logger) = &mut self.logger {
            logger.reset_core(core)?;
        }
        Ok(())
    }

    pub(super) fn erase_core_prompt(&mut self, core: CoreId) -> std::io::Result<()> {
        if self
            .state
            .foreground()
            .is_some_and(|line| line.channel.core == core)
        {
            erase_foreground(&mut self.state, &mut self.output)?;
        }
        Ok(())
    }

    pub(super) fn reset_core_epoch(&mut self, core: CoreId) -> Result<()> {
        self.finish_core_epoch(core)?;
        self.state.reset_core(core);
        Ok(())
    }

    pub(super) fn select_down_core(&mut self, core: Option<CoreId>) -> std::io::Result<()> {
        if self.state.selected_down_core() != core
            && self
                .state
                .foreground()
                .is_some_and(|line| Some(line.channel.core) != core)
        {
            erase_foreground(&mut self.state, &mut self.output)?;
        }
        self.state.select_down_core(core);
        Ok(())
    }

    fn finish_redirected_partials(&mut self) -> std::io::Result<()> {
        debug_assert!(!self.state.is_interactive());

        let mut partials: Vec<_> = match &self.state.presentation {
            Presentation::Redirected { partials } => partials
                .iter()
                .filter(|(_, display)| !display.is_empty())
                .map(|(&channel, display)| (channel, display.clone()))
                .collect(),
            Presentation::Interactive(_) => {
                unreachable!("interactive output has no redirected partials")
            }
        };
        partials.sort_unstable_by_key(|(channel, _)| *channel);
        self.emit_redirected_partials(partials)
    }

    fn finish_redirected_partials_for(&mut self, core: CoreId) -> std::io::Result<()> {
        debug_assert!(!self.state.is_interactive());

        let mut partials: Vec<_> = match &self.state.presentation {
            Presentation::Redirected { partials } => partials
                .iter()
                .filter(|(source, display)| source.core == core && !display.is_empty())
                .map(|(&channel, display)| (channel, display.clone()))
                .collect(),
            Presentation::Interactive(_) => {
                unreachable!("interactive output has no redirected partials")
            }
        };
        partials.sort_unstable_by_key(|(channel, _)| *channel);
        self.emit_redirected_partials(partials)
    }

    fn emit_redirected_partials(
        &mut self,
        partials: Vec<(CoreChannel, Vec<u8>)>,
    ) -> std::io::Result<()> {
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

    fn render_terminal_event(
        &mut self,
        source: CoreChannel,
        chunk: TerminalChunk,
        timestamp: Instant,
    ) -> Result<()> {
        render_terminal_event(
            source,
            chunk,
            timestamp,
            &mut self.state,
            self.logger.as_mut(),
            &mut self.output,
        )
    }

    fn render_defmt_frame(
        &mut self,
        source: CoreChannel,
        frame: &DecodedFrame,
        timestamp: Instant,
    ) -> Result<()> {
        render_defmt_frame(
            source,
            frame,
            timestamp,
            &mut self.state,
            self.logger.as_mut(),
            &mut self.output,
        )
    }

    fn render_defmt_warning(
        &mut self,
        source: CoreChannel,
        warning: &str,
        timestamp: Instant,
    ) -> Result<()> {
        render_defmt_warning(
            source,
            warning,
            timestamp,
            &mut self.state,
            self.logger.as_mut(),
            &mut self.output,
        )
    }

    fn log_raw_bytes(&mut self, source: CoreChannel, bytes: &[u8]) -> Result<()> {
        if let Some(logger) = self.logger.as_mut() {
            logger.write_bytes(source, bytes)?;
        }
        Ok(())
    }

    pub(super) fn flush_data(&mut self) -> Result<()> {
        if let Some(logger) = &mut self.logger {
            logger.flush_files()?;
        }
        self.output.flush().context("Error writing to stdout")
    }

    pub(super) fn flush_output(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }

    pub(super) fn finish_session(&mut self) -> Result<()> {
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

    pub(super) fn show_banner(&mut self) -> std::io::Result<()> {
        if self.state.is_interactive() {
            write_session_banner(&mut self.output)?;
        }
        Ok(())
    }

    pub(super) fn show_help(&mut self) -> std::io::Result<()> {
        write_help(&mut self.output)?;
        self.state.line_start = true;
        Ok(())
    }

    pub(super) fn show_config(
        &mut self,
        cores: &[CoreId],
        identity: &ProbeIdentity,
        config: &SessionPolicy,
        down_target: Option<CoreId>,
    ) -> std::io::Result<()> {
        write_config(
            cores,
            identity,
            config,
            down_target,
            &self.state,
            &mut self.output,
        )?;
        self.state.line_start = true;
        Ok(())
    }

    pub(super) fn clear_screen(&mut self) -> std::io::Result<()> {
        clear_screen(&mut self.output)?;
        self.state.line_start = true;
        Ok(())
    }

    pub(super) fn toggle_timestamps(&mut self) -> std::io::Result<()> {
        self.state.timestamps = !self.state.timestamps;
        write_toggle_status(&mut self.output, "Timestamps", self.state.timestamps)?;
        self.output.flush()?;
        self.state.line_start = true;
        Ok(())
    }

    pub(super) fn notice_target_reset(&mut self) -> std::io::Result<()> {
        write!(self.output, "\r\nTarget reset.\r\n")?;
        self.output.flush()?;
        self.state.line_start = true;
        Ok(())
    }

    pub(super) fn notice_reattached(&mut self, core: CoreId) -> std::io::Result<()> {
        if self.state.is_interactive() {
            write!(self.output, "\r\nRTT reattached to core {core}.\r\n")?;
            self.output.flush()?;
        }
        Ok(())
    }

    pub(super) fn notice_down_target(
        &mut self,
        core: CoreId,
        channel: ChannelId,
    ) -> std::io::Result<()> {
        write!(
            self.output,
            "\r\nKeyboard input now targets core {core} (down ch{channel}).\r\n"
        )?;
        self.output.flush()?;
        self.state.line_start = true;
        Ok(())
    }

    pub(super) fn show_cached_prompt(&mut self, core: CoreId) -> std::io::Result<bool> {
        let cached = match &self.state.presentation {
            Presentation::Interactive(state) => state
                .prompts_by_core
                .get(&core)
                .and_then(|prompt| prompt.channels.last())
                .and_then(|channel| state.prompts.get(channel))
                .cloned(),
            Presentation::Redirected { .. } => None,
        };
        let Some(cached) = cached else {
            return Ok(false);
        };
        if cached.rendered.is_empty() {
            return Ok(false);
        }
        let _ = self.suspend_foreground()?;
        self.restore_foreground(cached)?;
        Ok(true)
    }

    pub(super) fn suspend_foreground(&mut self) -> std::io::Result<Option<ForegroundLine>> {
        erase_foreground(&mut self.state, &mut self.output)
    }

    pub(super) fn key_pressed(&mut self, at: Instant) -> std::io::Result<()> {
        let core = self
            .state
            .selected_down_core()
            .or_else(|| self.state.foreground().map(|line| line.channel.core));
        if let Some(core) = core {
            self.state.record_prompt_time(core, at);
            if !self.state.timestamps {
                return Ok(());
            }
            if let Some(saved) = self.suspend_foreground()? {
                self.restore_foreground(saved)?;
                self.output.flush()?;
            }
        }
        Ok(())
    }

    pub(super) fn restore_foreground(&mut self, saved: ForegroundLine) -> std::io::Result<()> {
        self.state.line_start = true;
        self.state.last_channel = None;
        render_channel_bytes(
            &saved.rendered,
            saved.channel,
            self.state.prompt_time(saved.channel.core, Instant::now()),
            &mut self.state,
            &mut self.output,
            None,
        )?;
        self.state.set_foreground(saved);
        Ok(())
    }
}

impl UpSink for Renderer {
    fn raw_bytes(&mut self, source: CoreChannel, bytes: &[u8]) -> Result<()> {
        self.log_raw_bytes(source, bytes)
    }

    fn terminal(&mut self, source: CoreChannel, chunk: TerminalChunk, at: Instant) -> Result<()> {
        self.render_terminal_event(source, chunk, at)
            .context("Error rendering terminal output")
    }

    fn defmt_frame(
        &mut self,
        source: CoreChannel,
        frame: &DecodedFrame,
        at: Instant,
    ) -> Result<()> {
        self.render_defmt_frame(source, frame, at)
            .context("Error rendering defmt frame")
    }

    fn defmt_warning(&mut self, source: CoreChannel, warning: &str, at: Instant) -> Result<()> {
        self.render_defmt_warning(source, warning, at)
            .context("Error rendering defmt warning")
    }

    fn needs_styled_terminal_output(&self) -> bool {
        self.state.is_interactive()
    }
}

fn erase_foreground(
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<Option<ForegroundLine>> {
    let foreground = state.take_foreground();
    if foreground.is_some() && state.is_interactive() {
        output.write_all(ERASE_CURRENT_LINE)?;
        output.write_all(ANSI_RESET)?;
    }
    state.line_start = true;
    state.last_channel = None;
    Ok(foreground)
}

fn render_channel_bytes(
    bytes: &[u8],
    source: CoreChannel,
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
        let channel_switch = state.last_channel != Some(source) && state.channel_labels;
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
                write!(output, "{}", channel_color(source).ansi())?;
            }
            if state.show_cores {
                write!(output, "{source} ")?;
            } else {
                write!(output, "[ch{}] ", source.channel)?;
            }
            if state.color {
                output.write_all(ANSI_RESET)?;
            }
            state.last_channel = Some(source);
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
    source: CoreChannel,
    chunk: TerminalChunk,
    timestamp: Instant,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let TerminalChunk { lines, partial } = chunk;
    for line in &lines {
        let foreground = erase_foreground(state, output)?;
        let rendered = if state.is_interactive() {
            &line.styled
        } else {
            line.plain.strip_suffix(b"\n").unwrap_or(&line.plain)
        };
        render_channel_bytes(rendered, source, timestamp, state, output, None)?;
        if state.is_interactive() {
            output.write_all(ANSI_RESET)?;
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
        state.remove_prompt(source);
        if let Some(saved) = foreground {
            if saved.channel != source {
                render_channel_bytes(
                    &saved.rendered,
                    saved.channel,
                    state.prompt_time(saved.channel.core, timestamp),
                    state,
                    output,
                    None,
                )?;
                state.set_foreground(saved);
            }
        }
    }

    if !state.is_interactive() {
        let Presentation::Redirected { partials } = &mut state.presentation else {
            unreachable!()
        };
        partials.insert(source, partial.plain);
        return Ok(());
    }
    if partial.styled.is_empty() {
        if state.foreground_is(source) {
            erase_foreground(state, output)?;
        }
        state.remove_prompt(source);
        return Ok(());
    }
    let prompt_time = state.prompt_time_or_record(source.core, timestamp);
    if state.foreground_is(source) {
        erase_foreground(state, output)?;
    } else if state
        .selected_down_core()
        .is_some_and(|owner| owner != source.core)
        || state.foreground().is_some()
    {
        // The selected down core owns the screen even when its line has
        // completed or been cleared.
        state.cache_prompt(ForegroundLine {
            channel: source,
            rendered: partial.styled,
        });
        return Ok(());
    }
    render_channel_bytes(&partial.styled, source, prompt_time, state, output, None)?;
    state.set_foreground(ForegroundLine {
        channel: source,
        rendered: partial.styled,
    });
    Ok(())
}
fn render_complete_line(
    source: CoreChannel,
    bytes: &[u8],
    timestamp: Instant,
    color: Option<&'static str>,
    state: &mut SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    let foreground = erase_foreground(state, output)?;
    render_channel_bytes(bytes, source, timestamp, state, output, color)?;
    if !state.line_start {
        if state.is_interactive() {
            output.write_all(b"\r\n")?;
        } else {
            output.write_all(b"\n")?;
        }
        state.line_start = true;
    }
    if let Some(saved) = foreground {
        render_channel_bytes(
            &saved.rendered,
            saved.channel,
            state.prompt_time(saved.channel.core, timestamp),
            state,
            output,
            None,
        )?;
        state.set_foreground(saved);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChannelColor {
    Cyan,
    Magenta,
    Blue,
    Green,
    Yellow,
    Red,
    BrightCyan,
    BrightMagenta,
    BrightBlue,
    BrightGreen,
    BrightYellow,
    BrightRed,
}

impl ChannelColor {
    const fn ansi(self) -> &'static str {
        match self {
            Self::Cyan => "\x1b[36m",
            Self::Magenta => "\x1b[35m",
            Self::Blue => "\x1b[34m",
            Self::Green => "\x1b[32m",
            Self::Yellow => "\x1b[33m",
            Self::Red => "\x1b[31m",
            Self::BrightCyan => "\x1b[96m",
            Self::BrightMagenta => "\x1b[95m",
            Self::BrightBlue => "\x1b[94m",
            Self::BrightGreen => "\x1b[92m",
            Self::BrightYellow => "\x1b[93m",
            Self::BrightRed => "\x1b[91m",
        }
    }
}

const CHANNEL_COLOR_PALETTE: [ChannelColor; 12] = [
    ChannelColor::Cyan,
    ChannelColor::Magenta,
    ChannelColor::Blue,
    ChannelColor::Green,
    ChannelColor::Yellow,
    ChannelColor::Red,
    ChannelColor::BrightCyan,
    ChannelColor::BrightMagenta,
    ChannelColor::BrightBlue,
    ChannelColor::BrightGreen,
    ChannelColor::BrightYellow,
    ChannelColor::BrightRed,
];

fn channel_color(source: CoreChannel) -> ChannelColor {
    CHANNEL_COLOR_PALETTE[(source.core.as_usize() % CHANNEL_COLOR_PALETTE.len()
        + source.channel.as_usize() % CHANNEL_COLOR_PALETTE.len())
        % CHANNEL_COLOR_PALETTE.len()]
}

fn render_terminal_event(
    source: CoreChannel,
    chunk: TerminalChunk,
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> Result<()> {
    if let Some(logger) = logger {
        logger.write_terminal_decoded(
            source,
            chunk.lines.iter().map(|line| line.plain.as_slice()),
            &chunk.partial.plain,
        )?;
    }
    Ok(render_terminal_chunk(
        source, chunk, timestamp, state, output,
    )?)
}

fn render_defmt_frame(
    source: CoreChannel,
    frame: &DecodedFrame,
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> Result<()> {
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
        logger.write_defmt_decoded(source, line.as_bytes())?;
    }
    let level_color = if state.color {
        let color = defmt_level_color(frame.level);
        (!color.is_empty()).then_some(color)
    } else {
        None
    };
    Ok(render_complete_line(
        source,
        line.as_bytes(),
        timestamp,
        level_color,
        state,
        output,
    )?)
}

fn render_defmt_warning(
    source: CoreChannel,
    warning: &str,
    timestamp: Instant,
    state: &mut SessionState,
    logger: Option<&mut Logger>,
    output: &mut impl Write,
) -> Result<()> {
    state.defmt_decode_warnings += 1;
    let line = format!(
        "[defmt warning #{}] {warning}\n",
        state.defmt_decode_warnings
    );
    if let Some(logger) = logger {
        logger.write_defmt_decoded(source, line.as_bytes())?;
    }
    Ok(render_complete_line(
        source,
        line.as_bytes(),
        timestamp,
        None,
        state,
        output,
    )?)
}

fn defmt_level_color(level: Option<defmt_parser::Level>) -> &'static str {
    match level {
        Some(defmt_parser::Level::Error) => "\x1b[31m",
        Some(defmt_parser::Level::Warn) => "\x1b[33m",
        Some(defmt_parser::Level::Debug | defmt_parser::Level::Trace) => "\x1b[2m",
        _ => "",
    }
}

fn write_help(output: &mut impl Write) -> std::io::Result<()> {
    write!(
        output,
        "\r\nCtrl-T commands:\r\n  q  Quit\r\n  ?  Show this help\r\n  c  Show configuration\r\n  l  Clear screen\r\n  t  Toggle timestamps\r\n  R  Reset target\r\n  d  Switch down-channel core\r\n  Ctrl-T  Send a literal Ctrl-T\r\n\r\nCtrl-C is sent to the target.\r\n\r\n"
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
    cores: &[CoreId],
    identity: &ProbeIdentity,
    config: &SessionPolicy,
    down_target: Option<CoreId>,
    state: &SessionState,
    output: &mut impl Write,
) -> std::io::Result<()> {
    write!(output, "\r\nConfiguration:\r\n")?;
    write!(output, "  Probe: {}\r\n", identity.label)?;
    write!(output, "  Chip: {}\r\n", identity.chip)?;
    for core in cores {
        write!(output, "  Core {core}: up")?;
        for spec in config
            .up_specs
            .iter()
            .filter(|spec| spec.applies_to(*core))
        {
            write!(output, " {}:{}", spec.channel, spec.mode.name())?;
        }
        write!(output, "\r\n")?;
    }
    if let Some(down_channel) = config.down_channel {
        write!(output, "  Down channel: {down_channel}\r\n")?;
        match down_target {
            Some(core) => write!(output, "  Down target: core {core}\r\n")?,
            None => write!(output, "  Down target: none\r\n")?,
        }
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
