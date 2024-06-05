use std::io;
use std::rc::Rc;

use crossterm::{cursor, queue, style, style::Stylize};

use crate::keymap::{Keybindings, UserAction};

use super::AppColors;

#[derive(Debug)]
pub struct KeybindingsWin {
    keymap: Keybindings,
    colors: Rc<AppColors>,
    start_row: u16,
    total_cols: u16,
}

impl KeybindingsWin {
    pub fn new(
        keymap: &Keybindings, colors: Rc<AppColors>, start_row: u16, total_cols: u16,
    ) -> Self {
        Self {
            keymap: keymap.clone(),
            colors,
            start_row,
            total_cols,
        }
    }

    pub fn redraw(&self) {
        let actions = vec![
            (UserAction::Quit, "Quit"),
            (UserAction::Help, "Help"),
            (UserAction::Enter, "Open podcast/Play episode"),
            (UserAction::SyncAll, "Refresh podcasts"),
            (UserAction::SyncGpodder, "Sync"),
            (UserAction::MarkPlayed, "Mark as played"),
            (UserAction::AddFeed, "Add podcast"),
            (UserAction::Enqueue, "Enqueue"),
            (UserAction::Remove, "Remove"),
        ];
        let mut cur_length = -3;
        let mut key_strs = Vec::new();
        for (action, action_str) in actions {
            let keys = self.keymap.keys_for_action(action);
            // longest prefix is 21 chars long
            let key_str = match keys.len() {
                0 => format!(":{}", action_str),
                _ => format!("{}:{}", &keys[0], action_str,),
            };
            if cur_length + key_str.len() as i16 + 3 > self.total_cols as i16 {
                break;
            }
            cur_length += key_str.len() as i16 + 3;
            key_strs.push(key_str);
        }
        let message = key_strs.join(" | ");
        let m0len = if self.total_cols as usize >= message.len() {
            self.total_cols as usize - message.len()
        } else {
            0
        };
        let message = message + &" ".repeat(m0len);
        queue!(
            io::stdout(),
            cursor::MoveTo(0, self.start_row),
            style::PrintStyledContent(
                style::style(&message)
                    .with(self.colors.normal.0)
                    .on(self.colors.normal.1)
            )
        )
        .unwrap();
    }

    pub fn resize(&mut self, start_row: u16, total_cols: u16) {
        self.start_row = start_row;
        self.total_cols = total_cols;

        self.redraw();
    }
}
