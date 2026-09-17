use crate::channel::{ChannelId, CoreId};
use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use std::collections::HashSet;
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
pub(crate) struct DownRoutes {
    target: CoreId,
    routes: Vec<DownRoute>,
}

struct DownRoute {
    core: CoreId,
    buffer: DownBuffer,
}

impl DownRoutes {
    fn new(routable: &[CoreId]) -> Self {
        let mut routes = Self {
            target: routable[0],
            routes: Vec::new(),
        };
        routes.set_routable(routable);
        routes
    }

    pub(crate) fn target(&self) -> CoreId {
        self.target
    }

    pub(crate) fn routable(&self) -> Vec<CoreId> {
        self.routes.iter().map(|route| route.core).collect()
    }

    fn current_mut(&mut self) -> &mut DownRoute {
        let target = self.target;
        self.routes
            .iter_mut()
            .find(|route| route.core == target)
            .expect("down target is always routable")
    }

    pub(crate) fn cycle(&mut self) -> CoreId {
        self.target = next_routable(&self.routable(), self.target);
        self.target
    }

    pub(crate) fn set_routable(&mut self, routable: &[CoreId]) -> Vec<(CoreId, usize)> {
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

    pub(crate) fn queue(&mut self, bytes: &[u8]) {
        self.current_mut().buffer.push(bytes);
    }

    pub(crate) fn flush_pending(
        &mut self,
        mut write: impl FnMut(CoreId, &mut [u8]) -> Result<usize>,
    ) -> Result<()> {
        for route in &mut self.routes {
            if route.buffer.is_empty() {
                continue;
            }
            let count = write(route.core, route.buffer.writable())?;
            route.buffer.consume(count);
        }
        Ok(())
    }

    fn clear_all(&mut self) {
        for route in &mut self.routes {
            route.buffer.clear();
        }
    }
}

/// Keyboard input mode for the interactive session, including typed bytes that
/// are waiting to be written to each routable core's down channel.
pub(crate) struct InteractiveInput {
    escape_state: EscapeState,
    routes: DownRoutes,
    _raw_mode: RawModeGuard,
}

pub(crate) fn next_routable(routable: &[CoreId], current: CoreId) -> CoreId {
    let position = routable
        .iter()
        .position(|&core| core == current)
        .expect("down target is always routable");
    routable[(position + 1) % routable.len()]
}

impl InteractiveInput {
    fn new(down_channel: Option<ChannelId>, routable: &[CoreId]) -> Result<Option<Self>> {
        if down_channel.is_none() || routable.is_empty() || !std::io::stdin().is_terminal() {
            return Ok(None);
        }

        terminal::enable_raw_mode()?;
        let mut routes = DownRoutes::new(routable);
        routes.queue(b"\n");
        Ok(Some(Self {
            escape_state: EscapeState::Normal,
            routes,
            _raw_mode: RawModeGuard,
        }))
    }

    fn poll_key(timeout: Duration) -> Result<Option<KeyEvent>> {
        if event::poll(timeout)? {
            if let Event::Key(key_event) = event::read()? {
                return Ok(Some(key_event));
            }
        }
        Ok(None)
    }
}

/// All session-owned state needed to route keyboard input to RTT down channels.
pub(crate) struct DownRouting {
    input: Option<InteractiveInput>,
    missing_warned: HashSet<CoreId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RoutingEvent {
    Switched { previous: CoreId, target: CoreId },
}

impl DownRouting {
    pub(crate) fn new() -> Self {
        Self {
            input: None,
            missing_warned: HashSet::new(),
        }
    }

    pub(crate) fn reconcile(
        &mut self,
        routable: &[CoreId],
        missing_down: &[CoreId],
        all_attached: bool,
        down_channel: Option<ChannelId>,
        down_explicit: bool,
    ) -> Result<Option<RoutingEvent>> {
        if routable.is_empty() {
            if down_explicit && all_attached {
                if let Some(down) = down_channel {
                    bail!("down channel {down} does not exist on any configured core");
                }
            }
            if self.input.is_some() {
                let reason = if all_attached {
                    "down channel unavailable on all cores"
                } else {
                    "down channel temporarily unavailable"
                };
                log::info!("{reason}; disabling keyboard input");
                self.input = None;
            }
            return Ok(None);
        }

        let mut switched = None;
        match self.input.as_mut() {
            None => self.input = InteractiveInput::new(down_channel, routable)?,
            Some(input) => {
                let previous = input.routes.target();
                for (core, dropped) in input.routes.set_routable(routable) {
                    log::warn!(
                        "core {core} lost its down channel; dropping {dropped} queued byte(s)"
                    );
                }
                let target = input.routes.target();
                if target != previous {
                    log::warn!(
                        "keyboard input moved to core {target} (previous target lost its down channel)"
                    );
                    switched = Some(RoutingEvent::Switched { previous, target });
                }
            }
        }

        if down_explicit {
            let missing: HashSet<CoreId> = missing_down.iter().copied().collect();
            for core in missing.difference(&self.missing_warned) {
                let down = down_channel.expect("explicit down selects a channel");
                log::warn!(
                    "core {core} has no down channel {down}; keyboard input unavailable there"
                );
            }
            self.missing_warned = missing;
        }
        Ok(switched)
    }

    pub(crate) fn wait_for_key(&self, timeout: Duration) -> Result<Option<KeyEvent>> {
        match self.input {
            Some(_) => InteractiveInput::poll_key(timeout),
            None => {
                std::thread::sleep(timeout);
                Ok(None)
            }
        }
    }

    pub(crate) fn handle_key(&mut self, key_event: KeyEvent) -> InputAction {
        let Some(input) = self.input.as_mut() else {
            return InputAction::Ignore;
        };
        let (next_state, action) = input.escape_state.handle_key(key_event);
        input.escape_state = next_state;
        action
    }

    pub(crate) fn queue(&mut self, bytes: &[u8]) {
        if let Some(input) = self.input.as_mut() {
            input.routes.queue(bytes);
        }
    }

    pub(crate) fn target(&self) -> Option<CoreId> {
        self.input.as_ref().map(|input| input.routes.target())
    }

    pub(crate) fn cycle(&mut self) -> Option<(CoreId, CoreId)> {
        let input = self.input.as_mut()?;
        let previous = input.routes.target();
        Some((previous, input.routes.cycle()))
    }

    pub(crate) fn clear_all(&mut self) {
        if let Some(input) = self.input.as_mut() {
            input.routes.clear_all();
        }
    }

    pub(crate) fn flush_pending(
        &mut self,
        write: impl FnMut(CoreId, &mut [u8]) -> Result<usize>,
    ) -> Result<()> {
        if let Some(input) = self.input.as_mut() {
            input.routes.flush_pending(write)?;
        }
        Ok(())
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
