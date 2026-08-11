//! A small, interruptible prompt shown while an agent response is streaming.
//!
//! Rustyline owns the ordinary between-turn prompt. During a response it is not
//! running, so this composer uses crossterm's asynchronous event stream. That
//! makes the reader cancellable (unlike a blocking stdin thread) and lets us
//! temporarily return terminal ownership to approval and elicitation prompts.

use anyhow::Result;
use crossterm::cursor::{MoveToColumn, MoveToNextLine, RestorePosition, SavePosition};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, Clear, ClearType};
use crossterm::{execute, queue};
use futures::StreamExt;
use std::io::{self, IsTerminal, Write};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LiveInputAction {
    Steer(String),
    Queue(String),
    Cancel,
}

pub(super) struct LiveInput {
    events: EventStream,
    composer: Composer,
    composer_visible: bool,
}

struct Composer {
    buffer: String,
    cursor: usize,
}

impl LiveInput {
    pub(super) fn start(buffer: String) -> Result<Option<Self>> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Ok(None);
        }
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let mut input = Self {
            events: EventStream::new(),
            composer: Composer {
                cursor: buffer.len(),
                buffer,
            },
            composer_visible: false,
        };
        input.redraw()?;
        Ok(Some(input))
    }

    pub(super) async fn next_action(&mut self) -> Result<Option<LiveInputAction>> {
        loop {
            let Some(event) = self.events.next().await else {
                return Ok(None);
            };
            match event? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if let Some(action) = self.composer.handle_key(key) {
                        self.redraw()?;
                        return Ok(Some(action));
                    }
                    self.redraw()?;
                }
                Event::Paste(text) => {
                    self.composer.buffer.insert_str(self.composer.cursor, &text);
                    self.composer.cursor += text.len();
                    self.redraw()?;
                }
                _ => {}
            }
        }
    }

    pub(super) fn clear_line(&mut self) -> Result<()> {
        if self.composer_visible {
            queue!(
                io::stdout(),
                MoveToColumn(0),
                Clear(ClearType::CurrentLine),
                RestorePosition
            )?;
            self.composer_visible = false;
        }
        io::stdout().flush()?;
        Ok(())
    }

    pub(super) fn redraw(&mut self) -> Result<()> {
        let prefix = "> ";
        let suffix = "  [Enter steer · Tab queue · Ctrl+C stop]";
        let displayed = display_fragment(&self.composer.buffer);
        let displayed_before_cursor = display_fragment(
            self.composer
                .buffer
                .get(..self.composer.cursor)
                .expect("composer cursor remains on a character boundary"),
        );
        if self.composer_visible {
            queue!(io::stdout(), MoveToColumn(0), Clear(ClearType::CurrentLine))?;
        } else {
            queue!(
                io::stdout(),
                SavePosition,
                MoveToNextLine(1),
                Clear(ClearType::CurrentLine)
            )?;
            self.composer_visible = true;
        }
        write!(io::stdout(), "{prefix}{displayed}{suffix}")?;
        let column = prefix.chars().count() + displayed_before_cursor.chars().count();
        queue!(
            io::stdout(),
            MoveToColumn(u16::try_from(column).unwrap_or(u16::MAX))
        )?;
        io::stdout().flush()?;
        Ok(())
    }

    pub(super) fn stop(mut self) -> Result<String> {
        self.clear_line()?;
        execute!(io::stdout(), DisableBracketedPaste)?;
        disable_raw_mode()?;
        Ok(std::mem::take(&mut self.composer.buffer))
    }
}

fn display_fragment(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' => '↵',
            '\t' => '⇥',
            character if character.is_control() => '�',
            character => character,
        })
        .collect()
}

impl Composer {
    fn handle_key(&mut self, key: KeyEvent) -> Option<LiveInputAction> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(LiveInputAction::Cancel);
        }
        match key.code {
            KeyCode::Enter if !self.buffer.trim().is_empty() => {
                let message = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                Some(LiveInputAction::Steer(message.trim().to_string()))
            }
            KeyCode::Tab if !self.buffer.trim().is_empty() => {
                let message = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                Some(LiveInputAction::Queue(message.trim().to_string()))
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.buffer.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                None
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let previous = self
                    .buffer
                    .get(..self.cursor)
                    .expect("composer cursor remains on a character boundary")
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                self.buffer.drain(previous..self.cursor);
                self.cursor = previous;
                None
            }
            KeyCode::Delete if self.cursor < self.buffer.len() => {
                let next = self
                    .buffer
                    .get(self.cursor..)
                    .expect("composer cursor remains on a character boundary")
                    .char_indices()
                    .nth(1)
                    .map(|(index, _)| self.cursor + index)
                    .unwrap_or(self.buffer.len());
                self.buffer.drain(self.cursor..next);
                None
            }
            KeyCode::Left if self.cursor > 0 => {
                self.cursor = self
                    .buffer
                    .get(..self.cursor)
                    .expect("composer cursor remains on a character boundary")
                    .char_indices()
                    .next_back()
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                None
            }
            KeyCode::Right if self.cursor < self.buffer.len() => {
                self.cursor = self
                    .buffer
                    .get(self.cursor..)
                    .expect("composer cursor remains on a character boundary")
                    .char_indices()
                    .nth(1)
                    .map(|(index, _)| self.cursor + index)
                    .unwrap_or(self.buffer.len());
                None
            }
            KeyCode::Home => {
                self.cursor = 0;
                None
            }
            KeyCode::End => {
                self.cursor = self.buffer.len();
                None
            }
            KeyCode::Esc => {
                self.buffer.clear();
                self.cursor = 0;
                None
            }
            _ => None,
        }
    }
}

impl Drop for LiveInput {
    fn drop(&mut self) {
        let _ = self.clear_line();
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn composer() -> Composer {
        Composer {
            buffer: String::new(),
            cursor: 0,
        }
    }

    #[test]
    fn enter_steers_and_tab_queues() {
        let mut input = composer();
        input.handle_key(key(KeyCode::Char('f')));
        input.handle_key(key(KeyCode::Char('i')));
        input.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            input.handle_key(key(KeyCode::Enter)),
            Some(LiveInputAction::Steer("fix".into()))
        );
        input.handle_key(key(KeyCode::Char('n')));
        input.handle_key(key(KeyCode::Char('e')));
        input.handle_key(key(KeyCode::Char('x')));
        input.handle_key(key(KeyCode::Char('t')));
        assert_eq!(
            input.handle_key(key(KeyCode::Tab)),
            Some(LiveInputAction::Queue("next".into()))
        );
    }

    #[test]
    fn unicode_editing_respects_character_boundaries() {
        let mut input = composer();
        input.handle_key(key(KeyCode::Char('é')));
        input.handle_key(key(KeyCode::Char('漢')));
        input.handle_key(key(KeyCode::Left));
        input.handle_key(key(KeyCode::Backspace));
        assert_eq!(input.buffer, "漢");
        assert_eq!(input.cursor, 0);
    }

    #[test]
    fn pasted_control_characters_are_safe_to_render() {
        assert_eq!(display_fragment("one\ntwo\t\u{1b}"), "one↵two⇥�");
    }
}
