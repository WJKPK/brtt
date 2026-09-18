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

/// Tracks only whether LF/VT/FF are executable terminal controls or payload
/// inside a 7-bit OSC/DCS/SOS/PM/APC string.
///
/// This deliberately does not parse CSI, decode UTF-8, recognize 8-bit C1
/// controls, or emulate terminal behavior. `vt100::Parser` does all terminal
/// parsing and rendering.
#[derive(Debug, Default, Clone, Copy)]
enum LineBoundaryState {
    #[default]
    Normal,
    Escape,
    Osc,
    ControlString,
}

impl LineBoundaryState {
    fn observe(&mut self, byte: u8) -> bool {
        const ESC: u8 = 0x1b;
        const CAN: u8 = 0x18;
        const SUB: u8 = 0x1a;

        let line_feed = matches!(byte, b'\n' | b'\x0b' | b'\x0c');
        match *self {
            Self::Normal => {
                if byte == ESC {
                    *self = Self::Escape;
                    false
                } else {
                    line_feed
                }
            }
            Self::Escape => match byte {
                ESC => false,
                CAN | SUB => {
                    *self = Self::Normal;
                    false
                }
                b']' => {
                    *self = Self::Osc;
                    false
                }
                b'P' | b'X' | b'^' | b'_' => {
                    *self = Self::ControlString;
                    false
                }
                byte if byte < 0x20 => line_feed,
                0x20..=0x7e => {
                    *self = Self::Normal;
                    false
                }
                _ => false,
            },
            Self::Osc => match byte {
                b'\x07' | CAN | SUB => {
                    *self = Self::Normal;
                    false
                }
                ESC => {
                    *self = Self::Escape;
                    false
                }
                _ => false,
            },
            Self::ControlString => match byte {
                CAN | SUB => {
                    *self = Self::Normal;
                    false
                }
                ESC => {
                    *self = Self::Escape;
                    false
                }
                _ => false,
            },
        }
    }
}

pub(crate) struct DecodedStream {
    parser: vt100::Parser,
    boundary: LineBoundaryState,
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        let mut parser = vt100::Parser::new(1, TERMINAL_BACKING_COLUMNS, 0);
        parser.process(DISABLE_LINE_WRAP);
        Self {
            parser,
            boundary: LineBoundaryState::default(),
        }
    }
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn consume_chunk(&mut self, bytes: &[u8], styled: bool) -> TerminalChunk {
        let mut lines = Vec::new();
        for &byte in bytes {
            let line_boundary = self.boundary.observe(byte);
            if line_boundary {
                lines.push(self.complete_line(styled));
            }
            self.process_parser_bytes(&[byte]);
            if line_boundary {
                self.process_parser_bytes(b"\r");
            }
        }
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
            // Backspace is executed without leaving Escape/CSI and is harmless
            // as payload when the parser is inside a terminal string.
            for _ in 0..(cursor - limit) {
                self.parser.process(b"\x08");
            }
        }
    }

    fn complete_line(&self, styled: bool) -> TerminalLine {
        let screen = self.parser.screen();
        let plain = screen
            .rows(0, MAX_TERMINAL_COLUMNS as u16)
            .next()
            .unwrap_or_default()
            .into_bytes();
        let styled_line = if styled {
            screen
                .rows_formatted(0, MAX_TERMINAL_COLUMNS as u16)
                .next()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let mut plain = plain;
        plain.push(b'\n');
        TerminalLine {
            plain,
            styled: styled_line,
        }
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
        if default {
            Vec::new()
        } else {
            screen.attributes_formatted()
        }
    }

    pub(crate) fn cursor_column(&self) -> usize {
        (self.parser.screen().cursor_position().1 as usize).min(MAX_TERMINAL_COLUMNS)
    }
}
