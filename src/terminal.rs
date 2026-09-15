//! Shared VT terminal model for live presentation and decoded logging.
//!
//! Each terminal channel is decoded once by [`DecodedStream::consume_chunk`].
//! The resulting plain decoded lines feed the log files while the
//! presentation-specific rendering feeds the interactive terminal, so the
//! decoded log view cannot disagree with what was displayed.
//! One logical line, not a screen.

const MAX_TERMINAL_COLUMNS: usize = 4096;
const TERMINAL_BACKING_COLUMNS: u16 = MAX_TERMINAL_COLUMNS as u16 + 2;
pub(crate) const MAX_RAW_LINE_BYTES: usize = 4096;
pub(crate) const MAX_RAW_ESCAPE_BYTES: usize = 32;

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
#[derive(Debug, Clone, Default)]
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

#[derive(Debug)]
struct CompletedRawLine {
    bytes: Vec<u8>,
    requires_terminal_rendering: bool,
}

#[derive(Debug)]
struct RawClassifier {
    line: Vec<u8>,
    state: RawInputState,
    requires_terminal_rendering: bool,
    completed: Vec<CompletedRawLine>,
}

impl RawClassifier {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            state: RawInputState::Text,
            requires_terminal_rendering: false,
            completed: Vec::new(),
        }
    }

    fn consume(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if matches!(self.state, RawInputState::PendingCr) {
                self.state = RawInputState::Text;
                if byte == b'\n' {
                    self.finish_line();
                    continue;
                }
                self.requires_terminal_rendering = true;
            }

            match byte {
                b'\r' => self.state = RawInputState::PendingCr,
                b'\n' => self.finish_line(),
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
        } else if len >= 3 && (0x40..=0x7e).contains(&byte) {
            if second != b'[' || byte != b'm' {
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

    fn finish_line(&mut self) {
        self.completed.push(CompletedRawLine {
            bytes: std::mem::take(&mut self.line),
            requires_terminal_rendering: self.requires_terminal_rendering,
        });
        self.state = RawInputState::Text;
        self.requires_terminal_rendering = false;
    }

    fn push_raw(&mut self, byte: u8) {
        if self.line.len() < MAX_RAW_LINE_BYTES {
            self.line.push(byte);
        } else {
            self.requires_terminal_rendering = true;
        }
    }
}

pub(crate) struct DecodedStream {
    parser: vt100::Parser,
    raw: RawClassifier,
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        let mut parser = vt100::Parser::new(1, TERMINAL_BACKING_COLUMNS, 0);
        parser.process(b"\x1b[?7l");
        Self {
            parser,
            raw: RawClassifier::new(),
        }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// Decodes `bytes` once, returning everything both downstream consumers
    /// need: plain lines for the log and presentation lines for the terminal.
    ///
    /// `styled` selects whether VT-styled presentation lines are computed
    /// (interactive mode). Redirected mode passes `false` to skip that work;
    /// simple lines still preserve their raw bytes in `display`.
    pub(crate) fn consume_chunk(&mut self, bytes: &[u8], styled: bool) -> TerminalChunk {
        self.raw.consume(bytes);
        let terminal_complete = self.consume_inner(bytes, styled);
        let raw_complete = std::mem::take(&mut self.raw.completed);
        debug_assert_eq!(
            raw_complete.len(),
            terminal_complete.len(),
            "raw and VT paths split lines differently"
        );

        let mut lines = Vec::with_capacity(terminal_complete.len());
        for (index, (log, styled_line)) in terminal_complete.into_iter().enumerate() {
            let simple = matches!(
                raw_complete.get(index),
                Some(CompletedRawLine {
                    requires_terminal_rendering: false,
                    ..
                })
            );
            let display = if simple {
                raw_complete[index].bytes.clone()
            } else if styled {
                styled_line
            } else {
                let mut plain = log.clone();
                if plain.last() == Some(&b'\n') {
                    plain.pop();
                }
                plain
            };
            lines.push(PresentedLine { log, display });
        }

        let log = self.visible_line();
        let (display, overlay) = if self.raw.requires_terminal_rendering {
            let plain = log.clone();
            (plain, self.rendered_partial())
        } else {
            let line = self.raw.line.clone();
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

    fn consume_inner(&mut self, bytes: &[u8], styled: bool) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut complete = Vec::new();
        let mut start = 0;
        while let Some(offset) = memchr::memchr(b'\n', &bytes[start..]) {
            let newline = start + offset;
            self.process_bounded(&bytes[start..newline]);
            let mut line = self.visible_line();
            line.push(b'\n');
            let styled_line = if styled {
                let mut styled = self.styled_visible_line();
                let attrs = self.active_attributes();
                if !styled.is_empty() && !attrs.is_empty() {
                    styled.extend_from_slice(b"\x1b[0m");
                }
                styled
            } else {
                Vec::new()
            };
            complete.push((line, styled_line));
            self.parser.process(b"\n");
            self.parser.process(b"\r");
            start = newline + 1;
        }
        self.process_bounded(&bytes[start..]);
        complete
    }

    fn process_bounded(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.parser.process(std::slice::from_ref(byte));
            if self.parser.screen().cursor_position().1 as usize > MAX_TERMINAL_COLUMNS {
                self.parser.process(b"\x1b[4097G");
            }
        }
    }

    fn rendered_partial(&self) -> Vec<u8> {
        let mut line = self.styled_visible_line();
        let cursor = self.cursor_column();
        let mut positioned = b"\x1b7".to_vec();
        positioned.append(&mut line);
        positioned.extend_from_slice(b"\x1b8");
        if cursor > 0 {
            positioned.extend_from_slice(format!("\x1b[{cursor}C").as_bytes());
        }
        positioned.extend_from_slice(&self.active_attributes());
        positioned
    }

    pub(crate) fn visible_line(&self) -> Vec<u8> {
        self.parser
            .screen()
            .rows(0, MAX_TERMINAL_COLUMNS as u16)
            .next()
            .unwrap_or_default()
            .into_bytes()
    }

    pub(crate) fn styled_visible_line(&self) -> Vec<u8> {
        self.parser
            .screen()
            .rows_formatted(0, MAX_TERMINAL_COLUMNS as u16)
            .next()
            .unwrap_or_default()
    }

    pub(crate) fn cursor_column(&self) -> usize {
        (self.parser.screen().cursor_position().1 as usize).min(MAX_TERMINAL_COLUMNS)
    }

    pub(crate) fn active_attributes(&self) -> Vec<u8> {
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
