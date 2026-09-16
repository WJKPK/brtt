use crate::channel::ChannelId;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use std::io::IsTerminal;
use std::time::Duration;

const MAX_DOWN_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscapeState {
    Normal,
    AwaitingCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionCommand {
    Quit,
    Help,
    ShowConfig,
    ClearScreen,
    ToggleTimestamps,
    ResetTarget,
    CycleDownCore,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputAction {
    Send(Vec<u8>),
    Command(SessionCommand),
    Ignore,
}

impl EscapeState {
    pub(crate) fn handle_key(self, key: KeyEvent) -> (Self, InputAction) {
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
                    KeyCode::Char('R') => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::ResetTarget),
                    ),
                    KeyCode::Char('d') if key.modifiers.is_empty() => (
                        EscapeState::Normal,
                        InputAction::Command(SessionCommand::CycleDownCore),
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

fn push_char(bytes: &mut Vec<u8>, character: char) {
    let mut buffer = [0u8; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

fn key_to_action(key: KeyEvent) -> InputAction {
    let mut bytes = Vec::new();

    match key.code {
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() =>
        {
            bytes.push((c.to_ascii_lowercase() as u8) & 0x1f);
        }
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::ALT) => {
            bytes.push(0x1b);
            push_char(&mut bytes, c);
        }
        KeyCode::Char(c) => push_char(&mut bytes, c),
        KeyCode::Enter => bytes.push(b'\n'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::Backspace => bytes.push(0x7f),
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

struct DownBuffer {
    bytes: Vec<u8>,
}

impl DownBuffer {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn push(&mut self, data: &[u8]) {
        let space = MAX_DOWN_BUFFER_BYTES.saturating_sub(self.bytes.len());
        if data.len() > space {
            let dropped = data.len() - space;
            log::warn!("down channel buffer is full; dropping {dropped} byte(s)");
            self.bytes.extend_from_slice(&data[..space]);
        } else {
            self.bytes.extend_from_slice(data);
        }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn clear(&mut self) {
        self.bytes.clear();
    }

    fn writable(&mut self) -> &mut [u8] {
        &mut self.bytes
    }

    fn consume(&mut self, count: usize) {
        assert!(
            count <= self.bytes.len(),
            "RTT wrote more bytes than provided"
        );
        self.bytes.drain(..count);
    }
}

/// Pending keyboard bytes per routable core. Bytes typed for one core stay
/// assigned to it across Ctrl-T d switches, so a partial write or an
/// unavailable core can never redirect them to the wrong destination.
/// Pure data: unit-tested without a terminal.
pub(crate) struct DownRoutes {
    target: u32,
    routes: Vec<DownRoute>,
}

struct DownRoute {
    core: u32,
    buffer: DownBuffer,
}

impl DownRoutes {
    fn new(routable: &[u32]) -> Self {
        let mut routes = Self {
            target: routable[0],
            routes: Vec::new(),
        };
        routes.set_routable(routable);
        routes
    }

    /// Core the keyboard currently writes to.
    pub(crate) fn target(&self) -> u32 {
        self.target
    }

    /// Routable cores in ascending order.
    pub(crate) fn routable(&self) -> Vec<u32> {
        self.routes.iter().map(|route| route.core).collect()
    }

    fn current_mut(&mut self) -> &mut DownRoute {
        let target = self.target;
        self.routes
            .iter_mut()
            .find(|route| route.core == target)
            .expect("down target is always routable")
    }

    /// Routes keyboard input to the next routable core. Queued bytes stay on
    /// their original routes; only the prompt-redraw newline goes to the new
    /// core's shell.
    pub(crate) fn cycle(&mut self) -> u32 {
        self.target = next_routable(&self.routable(), self.target);
        self.current_mut().buffer.push(b"\n");
        self.target
    }

    /// Refreshes the routable set after attach/reattach. Keeps the current
    /// target when still routable, else falls back to the lowest core.
    /// Returns removed cores that still held pending bytes, with the dropped
    /// counts; silently dropped empty routes are not reported.
    pub(crate) fn set_routable(&mut self, routable: &[u32]) -> Vec<(u32, usize)> {
        let mut removed = Vec::new();
        self.routes.retain(|route| {
            if routable.contains(&route.core) {
                true
            } else {
                if !route.buffer.is_empty() {
                    removed.push((route.core, route.buffer.len()));
                }
                false
            }
        });
        for core in routable {
            if !self.routes.iter().any(|route| route.core == *core) {
                self.routes.push(DownRoute {
                    core: *core,
                    buffer: DownBuffer::new(),
                });
            }
        }
        self.routes.sort_by_key(|route| route.core);
        if let Some(first) = self.routes.first() {
            if !self.routes.iter().any(|route| route.core == self.target) {
                self.target = first.core;
            }
        }
        removed
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.routes.iter().any(|route| !route.buffer.is_empty())
    }

    pub(crate) fn queue(&mut self, bytes: &[u8]) {
        self.current_mut().buffer.push(bytes);
    }

    /// Writes one route's pending bytes through `write`, consuming what the
    /// target accepted. No route or no pending bytes is a no-op.
    pub(crate) fn flush_route(
        &mut self,
        core: u32,
        write: impl FnOnce(&mut [u8]) -> Result<usize>,
    ) -> Result<()> {
        let Some(route) = self.routes.iter_mut().find(|route| route.core == core) else {
            return Ok(());
        };
        if route.buffer.is_empty() {
            return Ok(());
        }
        let count = write(route.buffer.writable())?;
        route.buffer.consume(count);
        Ok(())
    }

    pub(crate) fn clear_all(&mut self) {
        for route in &mut self.routes {
            route.buffer.clear();
        }
    }
}

/// Keyboard input mode for the interactive session, including typed bytes that
/// are waiting to be written to each routable core's down channel.
pub(crate) struct InteractiveInput {
    pub(crate) escape_state: EscapeState,
    routes: DownRoutes,
    _raw_mode: RawModeGuard,
}

/// Next routable core after `current`, wrapping around. Pure step behind
/// [`DownRoutes::cycle`], unit-tested directly.
pub(crate) fn next_routable(routable: &[u32], current: u32) -> u32 {
    let position = routable
        .iter()
        .position(|&core| core == current)
        .expect("down target is always routable");
    routable[(position + 1) % routable.len()]
}

impl InteractiveInput {
    /// Enables raw keyboard input when a down channel exists on at least one
    /// core and stdin is a terminal. Keyboard bytes go to the lowest routable
    /// core until switched with Ctrl-T d.
    pub(crate) fn new(down_channel: Option<ChannelId>, routable: &[u32]) -> Result<Option<Self>> {
        if down_channel.is_none() || routable.is_empty() || !std::io::stdin().is_terminal() {
            return Ok(None);
        }

        terminal::enable_raw_mode()?;
        let mut routes = DownRoutes::new(routable);
        // Ask the target shell to redraw its normal prompt at session start.
        routes.queue(b"\n");
        Ok(Some(Self {
            escape_state: EscapeState::Normal,
            routes,
            _raw_mode: RawModeGuard,
        }))
    }

    /// Core the keyboard currently writes to.
    pub(crate) fn down_target(&self) -> u32 {
        self.routes.target()
    }

    pub(crate) fn routable(&self) -> Vec<u32> {
        self.routes.routable()
    }

    /// Routes keyboard input to the next routable core. Queued bytes stay on
    /// their original routes; the new core's shell gets a prompt redraw.
    pub(crate) fn cycle_down_target(&mut self) -> u32 {
        self.routes.cycle()
    }

    /// Non-blocking key check with timeout. Returns `None` on timeout or when
    /// the pending event is not a key (e.g. resize). Knows nothing about RTT.
    pub(crate) fn poll_key(timeout: Duration) -> Result<Option<KeyEvent>> {
        if event::poll(timeout)? {
            if let Event::Key(key_event) = event::read()? {
                return Ok(Some(key_event));
            }
        }
        Ok(None)
    }

    pub(crate) fn set_routable(&mut self, routable: &[u32]) -> Vec<(u32, usize)> {
        self.routes.set_routable(routable)
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.routes.has_pending()
    }

    pub(crate) fn queue(&mut self, bytes: &[u8]) {
        self.routes.queue(bytes);
    }

    pub(crate) fn flush_route(
        &mut self,
        core: u32,
        write: impl FnOnce(&mut [u8]) -> Result<usize>,
    ) -> Result<()> {
        self.routes.flush_route(core, write)
    }

    pub(crate) fn clear_all(&mut self) {
        self.routes.clear_all();
    }
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(test)]
#[path = "../tests/input.rs"]
mod tests;
