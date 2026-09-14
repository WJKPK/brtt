//! Shared VT terminal model for decoded logging and live presentation.
//!
//! Both the log files and the interactive terminal consume the same model so
//! the decoded log view can never disagree with what was displayed.

const MAX_TERMINAL_COLUMNS: usize = 4096;
const TERMINAL_BACKING_COLUMNS: u16 = MAX_TERMINAL_COLUMNS as u16 + 2;

pub(crate) struct DecodedStream {
    parser: vt100::Parser,
}

impl DecodedStream {
    pub(crate) fn new() -> Self {
        let mut parser = vt100::Parser::new(1, TERMINAL_BACKING_COLUMNS, 0);
        parser.process(b"\x1b[?7l");
        Self { parser }
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn consume(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.consume_inner(bytes, false)
            .into_iter()
            .map(|(plain, _)| plain)
            .collect()
    }

    /// Like [`consume`](Self::consume), but also captures each completed line
    /// with inline SGR styling.
    ///
    /// Returns `(plain, styled)` pairs. Styled lines carry no trailing newline;
    /// when a styled line leaves terminal attributes active it is terminated
    /// with `\x1b[0m` so the bytes are self-contained.
    pub(crate) fn consume_styled(&mut self, bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.consume_inner(bytes, true)
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
                if !styled.is_empty() && !self.active_attributes().is_empty() {
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

    #[cfg(test)]
    pub(crate) fn cursor_back_from_end(&self) -> usize {
        let (_, cursor) = self.parser.screen().cursor_position();
        let end = (0..MAX_TERMINAL_COLUMNS)
            .rev()
            .find(|&column| {
                self.parser
                    .screen()
                    .cell(0, column as u16)
                    .is_some_and(|cell| !cell.contents().is_empty())
            })
            .map_or(0, |column| column + 1);
        end.saturating_sub(cursor as usize)
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
