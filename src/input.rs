use crate::channel::ChannelId;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use std::io::IsTerminal;
use std::time::Duration;

pub(crate) const MAX_DOWN_BUFFER_BYTES: usize = 64 * 1024;

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
    dropped: u64,
}

impl DownBuffer {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            dropped: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        let space = MAX_DOWN_BUFFER_BYTES.saturating_sub(self.bytes.len());
        if data.len() > space {
            let dropped = data.len() - space;
            self.dropped += dropped as u64;
            log::warn!("down channel buffer is full; dropping {dropped} byte(s)");
            self.bytes.extend_from_slice(&data[..space]);
        } else {
            self.bytes.extend_from_slice(data);
        }
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
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

/// Keyboard input mode for the interactive session, including typed bytes that
/// are waiting to be written to the target's down channel.
pub(crate) struct InteractiveInput {
    pub(crate) escape_state: EscapeState,
    down_buffer: DownBuffer,
    _raw_mode: RawModeGuard,
}

impl InteractiveInput {
    /// Enables raw keyboard input when a down channel exists and stdin is a
    /// terminal. `down_channel_present` reports whether the target exposes the
    /// channel selected by the configuration.
    pub(crate) fn new(
        down_channel: Option<ChannelId>,
        down_channel_present: bool,
    ) -> Result<Option<Self>> {
        if down_channel.is_none() || !down_channel_present || !std::io::stdin().is_terminal() {
            return Ok(None);
        }

        terminal::enable_raw_mode()?;
        let mut down_buffer = DownBuffer::new();
        // Ask the target shell to redraw its normal prompt at session start.
        down_buffer.push(b"\n");
        Ok(Some(Self {
            escape_state: EscapeState::Normal,
            down_buffer,
            _raw_mode: RawModeGuard,
        }))
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

    pub(crate) fn clear_queued_bytes(&mut self) {
        self.down_buffer.clear();
    }

    pub(crate) fn has_pending(&self) -> bool {
        !self.down_buffer.is_empty()
    }

    pub(crate) fn queue(&mut self, bytes: &[u8]) {
        self.down_buffer.push(bytes);
    }

    pub(crate) fn pending_bytes(&mut self) -> &mut [u8] {
        self.down_buffer.writable()
    }

    pub(crate) fn consume_sent(&mut self, count: usize) {
        self.down_buffer.consume(count);
    }
}

struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}
