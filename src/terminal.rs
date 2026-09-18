//! Shared VT terminal model for live presentation and decoded logging.
//!
//! Each terminal channel is decoded once by [`DecodedStream::consume_chunk`].
//! The resulting plain decoded lines feed the log files while the
//! presentation-specific rendering feeds the interactive terminal, so the
//! decoded log view cannot disagree with what was displayed.
//! One logical line, not a screen.

const MAX_TERMINAL_COLUMNS: usize = 4096;
const TERMINAL_BACKING_COLUMNS: u16 = MAX_TERMINAL_COLUMNS as u16 + 2;
const CURSOR_CLAMP_CMD: &[u8] = b"\x1b[4097G";
pub(crate) const ANSI_RESET: &[u8] = b"\x1b[0m";
pub(crate) const ERASE_CURRENT_LINE: &[u8] = b"\r\x1b[2K";
const DISABLE_LINE_WRAP: &[u8] = b"\x1b[?7l";
const CURSOR_SAVE: &[u8] = b"\x1b7";
const CURSOR_RESTORE: &[u8] = b"\x1b8";
const MAX_RAW_LINE_BYTES: usize = 4096;
pub(crate) const MAX_RAW_ESCAPE_BYTES: usize = 32;
const CSI_FINAL_BYTE_RANGE: std::ops::RangeInclusive<u8> = 0x40..=0x7e;
const SGR_FINAL_BYTE: u8 = b'm';

/// One completed terminal line, fully decoded once.
#[derive(Debug)]
pub(crate) struct PresentedLine {
    /// Plain VT-decoded line including trailing `\n`; the logger's only input.
    pub(crate) log: Vec<u8>,
    /// What the live terminal shows (no trailing newline): raw input bytes
    /// when the line is simple enough to reproduce byte-for-byte, otherwise
    /// VT-styled content in interactive mode or plain text when redirected.
    pub(crate) display: Vec<u8>,
}

/// The current incomplete line, fully decoded once.
#[derive(Debug)]
pub(crate) struct PartialView {
    /// Plain VT-decoded partial line for the log.
    pub(crate) log: Vec<u8>,
    /// Raw bytes when simple, otherwise plain text; used for emptiness checks
    /// and redirected finalization.
    pub(crate) display: Vec<u8>,
    /// What the interactive terminal actually draws: `display` for simple
    /// lines, otherwise styled content with cursor positioning and attributes.
    pub(crate) overlay: Vec<u8>,
}

/// One terminal channel's fully-decoded output for a single poll.
#[derive(Debug)]
pub(crate) struct TerminalChunk {
    pub(crate) lines: Vec<PresentedLine>,
    pub(crate) partial: PartialView,
}

#[derive(Debug)]
enum RawInputState {
    Text,
    PendingCr,
    Escape { len: usize, second: u8 },
}

/// The current incomplete line's raw-byte state. Observed one byte at a
/// time by [`DecodedStream::process_bounded`], in the same traversal that
/// feeds the VT parser, so classification never re-scans the input.
#[derive(Debug)]
struct RawLine {
    /// Raw bytes of the current line, capped at [`MAX_RAW_LINE_BYTES`].
    bytes: Vec<u8>,
    state: RawInputState,
    requires_terminal_rendering: bool,
}

impl Default for RawLine {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            state: RawInputState::Text,
            requires_terminal_rendering: false,
        }
    }
}

impl RawLine {
    /// Classifies one byte that is not a `\n` (newlines are handled only at
    /// the boundary in [`DecodedStream::consume_inner`]). Called immediately
    /// before the same byte reaches the VT parser.
    fn observe_byte(&mut self, byte: u8) {
        if matches!(self.state, RawInputState::PendingCr) {
            self.state = RawInputState::Text;
            self.requires_terminal_rendering = true;
        }

        match byte {
            b'\r' => self.state = RawInputState::PendingCr,
            b'\x1b' => {
                self.state = RawInputState::Escape { len: 1, second: 0 };
                self.push_raw(byte);
            }
            byte => {
                self.push_raw(byte);
                self.update_escape(byte);
            }
        }
    }
    /// Takes the completed line at a `\n` boundary. A `PendingCr` state here
    /// means CRLF, so the raw bytes stay eligible for direct display. The
    /// placeholder from [`std::mem::take`] resets for the next line.
    fn take_completed(&mut self) -> RawLine {
        std::mem::take(self)
    }

    fn update_escape(&mut self, byte: u8) {
        let (len, second) = match self.state {
            RawInputState::Escape { len, second } => (len, second),
            _ => return,
        };

        let len = if len < MAX_RAW_ESCAPE_BYTES {
            len + 1
        } else {
            self.requires_terminal_rendering = true;
            len
        };

        if len == 2 && byte != b'[' {
            self.requires_terminal_rendering = true;
            self.state = RawInputState::Text;
        } else if len >= 3 && CSI_FINAL_BYTE_RANGE.contains(&byte) {
            if second != b'[' || byte != SGR_FINAL_BYTE {
                self.requires_terminal_rendering = true;
            }
            self.state = RawInputState::Text;
        } else {
            self.state = RawInputState::Escape {
                len,
                second: if len == 2 { byte } else { second },
            };
        }
    }

    fn push_raw(&mut self, byte: u8) {
        if self.bytes.len() < MAX_RAW_LINE_BYTES {
            self.bytes.push(byte);
        } else {
            self.requires_terminal_rendering = true;
        }
    }
}

pub(crate) struct DecodedStream {
    parser: vt100::Parser,
    raw: RawLine,
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        let mut parser = vt100::Parser::new(1, TERMINAL_BACKING_COLUMNS, 0);
        parser.process(DISABLE_LINE_WRAP);
        Self {
            parser,
            raw: RawLine::default(),
        }
    }

    /// Decodes `bytes` once, returning everything both downstream consumers
    /// need: plain lines for the log and presentation lines for the terminal.
    ///
    /// `styled` selects whether VT-styled presentation lines are computed
    /// (interactive mode). Redirected mode passes `false` to skip that work;
    /// simple lines still preserve their raw bytes in `display`.
    pub(crate) fn consume_chunk(&mut self, bytes: &[u8], styled: bool) -> TerminalChunk {
        let lines = self.consume_inner(bytes, styled);

        let log = self.visible_line();
        let (display, overlay) = if self.raw.requires_terminal_rendering {
            let plain = log.clone();
            (plain, self.rendered_partial())
        } else {
            let line = self.raw.bytes.clone();
            (line.clone(), line)
        };
        TerminalChunk {
            lines,
            partial: PartialView {
                log,
                display,
                overlay,
            },
        }
    }

    fn consume_inner(&mut self, bytes: &[u8], styled: bool) -> Vec<PresentedLine> {
        let mut complete = Vec::new();
        let mut start = 0;
        while let Some(offset) = memchr::memchr(b'\n', &bytes[start..]) {
            let newline = start + offset;
            self.process_bounded(&bytes[start..newline]);
            let mut log = self.visible_line();
            log.push(b'\n');
            let styled_line = if styled {
                let mut styled = self.styled_visible_line();
                let attrs = self.active_attributes();
                if !styled.is_empty() && !attrs.is_empty() {
                    styled.extend_from_slice(ANSI_RESET);
                }
                styled
            } else {
                Vec::new()
            };
            let raw_line = self.raw.take_completed();
            let display = Self::resolve_display(raw_line, &log, styled_line, styled);
            complete.push(PresentedLine { log, display });
            self.parser.process(b"\n");
            self.parser.process(b"\r");
            start = newline + 1;
        }
        self.process_bounded(&bytes[start..]);
        complete
    }

    /// Chooses the presentation bytes for a completed line: the raw input
    /// when it is simple enough to reproduce byte-for-byte, the VT-styled
    /// render when interactive, or plain text without the newline otherwise.
    fn resolve_display(
        raw_line: RawLine,
        log: &[u8],
        styled_line: Vec<u8>,
        styled: bool,
    ) -> Vec<u8> {
        match (raw_line.requires_terminal_rendering, styled) {
            (false, _) => raw_line.bytes,
            (true, true) => styled_line,
            (true, false) => {
                let mut plain = styled_line;
                plain.extend_from_slice(log);
                if plain.last() == Some(&b'\n') {
                    plain.pop();
                }
                plain
            }
        }
    }
    /// Feeds each byte to the VT parser, clamping the cursor into the
    /// visible width. Clamping happens before a byte while the parser is in
    /// ground state, so an injected clamp never splits a pending escape
    /// sequence; printable overflows beyond the visible cap then saturate
    /// across the hidden backing columns.
    fn process_bounded(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let pending_escape = matches!(self.raw.state, RawInputState::Escape { .. });
            let column = self.parser.screen().cursor_position().1 as usize;
            if !pending_escape && column > MAX_TERMINAL_COLUMNS {
                // Control bytes (backspace, tab, CR, and every ESC-led
                // cursor command) must start from the fixed visible
                // boundary; printable saturation must not leave the
                // hidden backing row.
                if byte < 0x20 || byte == 0x7f || column > TERMINAL_BACKING_COLUMNS as usize - 1 {
                    self.parser.process(CURSOR_CLAMP_CMD);
                }
            }
            self.raw.observe_byte(byte);
            self.parser.process(std::slice::from_ref(&byte));
        }
        if self.parser.screen().cursor_position().1 as usize > MAX_TERMINAL_COLUMNS {
            self.parser.process(CURSOR_CLAMP_CMD);
        }
    }

    fn rendered_partial(&self) -> Vec<u8> {
        let mut line = self.styled_visible_line();
        let cursor = self.cursor_column();
        let mut positioned = CURSOR_SAVE.to_vec();
        positioned.append(&mut line);
        positioned.extend_from_slice(CURSOR_RESTORE);
        if cursor > 0 {
            positioned.extend_from_slice(format!("\x1b[{cursor}C").as_bytes());
        }
        positioned.extend_from_slice(&self.active_attributes());
        positioned
    }

    fn visible_line(&self) -> Vec<u8> {
        self.parser
            .screen()
            .rows(0, MAX_TERMINAL_COLUMNS as u16)
            .next()
            .unwrap_or_default()
            .into_bytes()
    }

    fn styled_visible_line(&self) -> Vec<u8> {
        self.parser
            .screen()
            .rows_formatted(0, MAX_TERMINAL_COLUMNS as u16)
            .next()
            .unwrap_or_default()
    }

    fn cursor_column(&self) -> usize {
        (self.parser.screen().cursor_position().1 as usize).min(MAX_TERMINAL_COLUMNS)
    }

    fn active_attributes(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let default = screen.fgcolor() == vt100::Color::Default
            && screen.bgcolor() == vt100::Color::Default
            && !screen.bold()
            && !screen.dim()
            && !screen.italic()
            && !screen.underline()
            && !screen.inverse();
        if default {
            Vec::new()
        } else {
            screen.attributes_formatted()
        }
    }
}

#[cfg(test)]
#[path = "../tests/terminal.rs"]
mod tests;
