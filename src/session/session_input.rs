use crate::channel::{ChannelId, CoreId};
use anyhow::{bail, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::time::Duration;

const MAX_DOWN_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Normal,
    AwaitingCommand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionCommand {
    Quit,
    Help,
    ShowConfig,
    ClearScreen,
    ToggleTimestamps,
    ResetTarget,
    CycleDownCore,
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

    fn clear(&mut self) -> usize {
        let discarded = self.bytes.len();
        self.bytes.clear();
        discarded
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NoRoutableDownChannel;

impl std::fmt::Display for NoRoutableDownChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no down channel available")
    }
}

impl std::error::Error for NoRoutableDownChannel {}

/// Pending keyboard bytes stay assigned to their original core.
/// Available cores currently have a routable down channel.
/// Selection survives a complete loss of availability.
struct DownRoutes {
    target: Option<CoreId>,
    routes: Vec<DownRoute>,
    available: Vec<CoreId>,
}

struct DownRoute {
    core: CoreId,
    buffer: DownBuffer,
}

impl DownRoutes {
    fn new(routable: &[CoreId]) -> Self {
        let mut routes = Self {
            target: None,
            routes: Vec::new(),
            available: Vec::new(),
        };
        routes.set_routable(routable);
        routes
    }

    /// The selected target can remain unavailable until a route returns.
    fn target(&self) -> Option<CoreId> {
        self.target
    }

    /// The active target is the selected target when it is available.
    fn active_target(&self) -> Option<CoreId> {
        self.target().filter(|core| self.available.contains(core))
    }

    fn current_mut(&mut self) -> &mut DownRoute {
        let target = self.target().expect("down target is selected");
        self.routes
            .iter_mut()
            .find(|route| route.core == target)
            .expect("down target is always routable")
    }

    fn cycle(&mut self) -> Option<CoreId> {
        if self.available.len() < 2 {
            return None;
        }
        let target = next_routable(
            &self.available,
            self.target().expect("down target is selected"),
        );
        self.target = Some(target);
        Some(target)
    }

    fn set_routable(&mut self, routable: &[CoreId]) {
        self.available.clear();
        self.available.extend_from_slice(routable);
        for core in routable {
            if !self.routes.iter().any(|route| route.core == *core) {
                self.routes.push(DownRoute {
                    core: *core,
                    buffer: DownBuffer::new(),
                });
            }
        }
        self.routes.sort_by_key(|route| route.core);
        if self.active_target().is_none() {
            if let Some(&first) = self.available.first() {
                self.target = Some(first);
            }
        }
    }

    fn queue(&mut self, bytes: &[u8]) -> std::result::Result<(), NoRoutableDownChannel> {
        if self.available.is_empty() {
            return Err(NoRoutableDownChannel);
        }
        self.current_mut().buffer.push(bytes);
        Ok(())
    }

    fn flush_pending(
        &mut self,
        mut write: impl FnMut(CoreId, &mut [u8]) -> Result<usize>,
    ) -> Result<()> {
        for route in &mut self.routes {
            if !self.available.contains(&route.core) {
                continue;
            }
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

    fn clear_core(&mut self, core: CoreId) -> usize {
        self.routes
            .iter_mut()
            .find(|route| route.core == core)
            .map_or(0, |route| route.buffer.clear())
    }
}

/// Keyboard input mode for the interactive session, including typed bytes that
/// are waiting to be written to each routable core's down channel.
struct InteractiveInput {
    escape_state: EscapeState,
    routes: DownRoutes,
    _raw_mode: RawModeGuard,
}

fn next_routable(routable: &[CoreId], current: CoreId) -> CoreId {
    let position = routable
        .iter()
        .position(|&core| core == current)
        .expect("down target is always routable");
    routable[(position + 1) % routable.len()]
}

impl InteractiveInput {
    fn new(down_channel: Option<ChannelId>, routable: &[CoreId]) -> Result<Option<Self>> {
        if down_channel.is_none() || !std::io::stdin().is_terminal() {
            return Ok(None);
        }

        terminal::enable_raw_mode()?;
        let routes = DownRoutes::new(routable);
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

/// Owns keyboard decoding, terminal mode, target selection, and pending bytes.
/// The session executes returned commands and supplies the hardware writer.
pub(super) struct SessionInput {
    down_channel: Option<ChannelId>,
    down_explicit: bool,
    input: Option<InteractiveInput>,
    missing_warned: HashSet<CoreId>,
    unavailable_warned: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RoutingEvent {
    Switched { previous: CoreId, target: CoreId },
}

impl SessionInput {
    pub(super) fn new(down_channel: Option<ChannelId>, down_explicit: bool) -> Self {
        Self {
            down_channel,
            down_explicit,
            input: None,
            missing_warned: HashSet::new(),
            unavailable_warned: false,
        }
    }

    pub(super) fn reconcile(
        &mut self,
        routable: &[CoreId],
        missing_down: &[CoreId],
        all_attached: bool,
    ) -> Result<Option<RoutingEvent>> {
        if routable.is_empty() {
            if self.down_explicit && all_attached {
                if let Some(down) = self.down_channel {
                    bail!("down channel {down} does not exist on any configured core");
                }
            }
            if let Some(input) = self.input.as_mut() {
                input.routes.set_routable(routable);
            } else {
                self.input = InteractiveInput::new(self.down_channel, routable)?;
            }
            return Ok(None);
        }

        self.unavailable_warned = false;
        let mut switched = None;
        match self.input.as_mut() {
            None => self.input = InteractiveInput::new(self.down_channel, routable)?,
            Some(input) => {
                let previous = input.routes.target();
                input.routes.set_routable(routable);
                let target = input.routes.target();
                if let (Some(previous), Some(target)) = (previous, target) {
                    if target != previous {
                        log::warn!(
                            "keyboard input moved to core {target} (previous target lost its down channel)"
                        );
                        switched = Some(RoutingEvent::Switched { previous, target });
                    }
                }
            }
        }

        if self.down_explicit {
            let missing: HashSet<CoreId> = missing_down.iter().copied().collect();
            for core in missing.difference(&self.missing_warned) {
                let down = self.down_channel.expect("explicit down selects a channel");
                log::warn!(
                    "core {core} has no down channel {down}; keyboard input unavailable there"
                );
            }
            self.missing_warned = missing;
        }
        Ok(switched)
    }

    pub(super) fn wait_for_key(&self, timeout: Duration) -> Result<Option<KeyEvent>> {
        match self.input {
            Some(_) => InteractiveInput::poll_key(timeout),
            None => {
                std::thread::sleep(timeout);
                Ok(None)
            }
        }
    }

    pub(super) fn handle_key(&mut self, key_event: KeyEvent) -> Option<SessionCommand> {
        let input = self.input.as_mut()?;
        let (next_state, action) = input.escape_state.handle_key(key_event);
        input.escape_state = next_state;
        match action {
            InputAction::Send(bytes) => {
                if input.routes.queue(&bytes).is_err() && !self.unavailable_warned {
                    log::warn!(
                        "no down channel available; keyboard input discarded until one returns"
                    );
                    self.unavailable_warned = true;
                }
                None
            }
            InputAction::Command(command) => Some(command),
            InputAction::Ignore => None,
        }
    }

    pub(super) fn active_target(&self) -> Option<CoreId> {
        self.input
            .as_ref()
            .and_then(|input| input.routes.active_target())
    }

    pub(super) fn cycle(&mut self) -> Option<RoutingEvent> {
        let input = self.input.as_mut()?;
        let previous = input.routes.target()?;
        let target = input.routes.cycle()?;
        Some(RoutingEvent::Switched { previous, target })
    }

    pub(super) fn clear_all(&mut self) {
        if let Some(input) = self.input.as_mut() {
            input.routes.clear_all();
        }
    }

    /// Discards bytes from the lost attachment and returns the discarded count.
    /// Call this before reconciling availability or processing more input.
    pub(super) fn attachment_lost(&mut self, core: CoreId) -> usize {
        self.input
            .as_mut()
            .map_or(0, |input| input.routes.clear_core(core))
    }

    pub(super) fn flush_pending(
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
