//! Shared VT terminal model for live presentation and decoded logging.

const MAX_TERMINAL_COLUMNS: usize = 4096;
const TERMINAL_BACKING_COLUMNS: u16 = MAX_TERMINAL_COLUMNS as u16 + 2;
const DISABLE_LINE_WRAP: &[u8] = b"\x1b[?7l";
pub(crate) const ANSI_RESET: &[u8] = b"\x1b[0m";
pub(crate) const ERASE_CURRENT_LINE: &[u8] = b"\r\x1b[2K";

#[derive(Debug)]
pub(crate) struct TerminalLine {
    pub(crate) plain: Vec<u8>,
    pub(crate) styled: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct TerminalChunk {
    pub(crate) lines: Vec<TerminalLine>,
    pub(crate) partial: TerminalLine,
}

#[derive(Debug, Default)]
struct LineFeedCallbacks {
    styled: bool,
    completed: Vec<TerminalLine>,
    input_state: InputState,
}

#[derive(Debug, Default, Clone, Copy)]
enum InputState {
    #[default]
    Ground,
    Escape,
    Csi,
    String,
    StringEscape,
}

impl InputState {
    fn observe(self, byte: u8) -> (Self, bool) {
        match self {
            Self::Ground => match byte {
                b'\n' | b'\x0b' | b'\x0c' => (Self::Ground, true),
                b'\x1b' => (Self::Escape, false),
                0x9b => (Self::Csi, false),
                0x90 | 0x98 | 0x9d | 0x9e | 0x9f => (Self::String, false),
                _ => (Self::Ground, false),
            },
            Self::Escape => match byte {
                b'[' => (Self::Csi, false),
                b']' | b'P' | b'^' | b'_' => (Self::String, false),
                b'\x1b' => (Self::Escape, false),
                _ => (Self::Ground, false),
            },
            Self::Csi => {
                if byte == b'\x1b' {
                    (Self::Escape, false)
                } else if (0x40..=0x7e).contains(&byte) {
                    (Self::Ground, false)
                } else {
                    (Self::Csi, false)
                }
            }
            Self::String => match byte {
                b'\x07' | 0x9c => (Self::Ground, false),
                b'\x1b' => (Self::StringEscape, false),
                _ => (Self::String, false),
            },
            Self::StringEscape => match byte {
                b'\\' => (Self::Ground, false),
                b'\x1b' => (Self::StringEscape, false),
                _ => (Self::String, false),
            },
        }
    }
}

impl vt100::Callbacks for LineFeedCallbacks {}

pub(crate) struct DecodedStream {
    parser: vt100::Parser<LineFeedCallbacks>,
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        let mut parser = vt100::Parser::new_with_callbacks(
            1,
            TERMINAL_BACKING_COLUMNS,
            0,
            LineFeedCallbacks::default(),
        );
        parser.process(DISABLE_LINE_WRAP);
        Self { parser }
    }

    pub(crate) fn consume_chunk(&mut self, bytes: &[u8], styled: bool) -> TerminalChunk {
        self.parser.callbacks_mut().styled = styled;
        for &byte in bytes {
            let line_feed = {
                let callbacks = self.parser.callbacks_mut();
                let (next_state, line_feed) = callbacks.input_state.observe(byte);
                callbacks.input_state = next_state;
                line_feed
            };
            if line_feed {
                self.complete_line();
            }
            self.process_parser_bytes(&[byte]);
            if line_feed {
                self.process_parser_bytes(b"\r");
            }
        }
        let lines = std::mem::take(&mut self.parser.callbacks_mut().completed);
        let partial = TerminalLine {
            plain: self.visible_line(),
            styled: if styled {
                self.rendered_partial()
            } else {
                Vec::new()
            },
        };
        TerminalChunk { lines, partial }
    }

    fn process_parser_bytes(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        let cursor = self.parser.screen().cursor_position().1;
        let limit = MAX_TERMINAL_COLUMNS as u16;
        if cursor > limit {
            let distance = cursor - limit;
            let sequence = format!("\x1b[{distance}D");
            self.parser.process(sequence.as_bytes());
        }
    }

    fn complete_line(&mut self) {
        let styled = self.parser.callbacks().styled;
        let screen = self.parser.screen();
        let plain = screen
            .rows(0, MAX_TERMINAL_COLUMNS as u16)
            .next()
            .unwrap_or_default()
            .into_bytes();
        let mut styled_line = if styled {
            screen
                .rows_formatted(0, MAX_TERMINAL_COLUMNS as u16)
                .next()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !styled_line.is_empty() {
            styled_line.extend_from_slice(ANSI_RESET);
        }
        let mut plain = plain;
        plain.push(b'\n');
        self.parser
            .callbacks_mut()
            .completed
            .push(TerminalLine {
                plain,
                styled: styled_line,
            });
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
    fn rendered_partial(&self) -> Vec<u8> {
        let mut line = self.styled_visible_line();
        let cursor = self.cursor_column();
        let plain_len = self.visible_line().len();
        if cursor == plain_len {
            line.extend_from_slice(&self.active_attributes());
            return line;
        }
        let mut positioned = b"\x1b7".to_vec();
        positioned.append(&mut line);
        positioned.extend_from_slice(b"\x1b8");
        if cursor > 0 {
            positioned.extend_from_slice(format!("\x1b[{cursor}C").as_bytes());
        }
        positioned.extend_from_slice(&self.active_attributes());
        positioned
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
        if default { Vec::new() } else { screen.attributes_formatted() }
    }

    pub(crate) fn cursor_column(&self) -> usize {
        (self.parser.screen().cursor_position().1 as usize).min(MAX_TERMINAL_COLUMNS)
    }

}
#[cfg(test)]
#[path = "../tests/terminal.rs"]
mod tests;
