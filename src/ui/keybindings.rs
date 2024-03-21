use std::io;
use std::rc::Rc;

use crossterm::{cursor, queue, style, style::Stylize};

use crate::keymap::{Keybindings, UserAction};

use super::AppColors;

#[derive(Debug)]
pub struct KeybindingsWin<'a> {
    keymap: &'a Keybindings,
    colors: Rc<AppColors>,
    start_y: u16,
    total_rows: u16,
    total_cols: u16,
}

impl<'a> KeybindingsWin<'a> {
    pub fn new(
        keymap: &'a Keybindings, colors: Rc<AppColors>, start_y: u16, total_rows: u16,
        total_cols: u16,
    ) -> Self {
        Self {
            keymap,
            colors,
            start_y,
            total_rows,
            total_cols,
        }
    }

    pub fn redraw(&self) {
        let actions = vec![
            (UserAction::Quit, "Quit"),
            (UserAction::Help, "Help"),
            (UserAction::Play, "Play"),
            (UserAction::SyncAll, "Sync all"),
            (UserAction::MarkPlayed, "Mark as played"),
            (UserAction::MarkAllPlayed, "Mark all as played"),
            (UserAction::Download, "Download"),
            (UserAction::Delete, "Delete file"),
            (UserAction::AddFeed, "Add feed"),
        ];
        let mut key_strs = Vec::new();
        for (action, action_str) in actions {
            let keys = self.keymap.keys_for_action(action);
            // longest prefix is 21 chars long
            let key_str = match keys.len() {
                0 => format!(":{}", action_str),
                _ => format!("{}:{}", &keys[0], action_str,),
            };
            key_strs.push(key_str);
        }
        let message0 = key_strs.join(" | ");
        let m0len = if self.total_cols as usize >= message0.len() {
            self.total_cols as usize - message0.len()
        } else {
            0
        };
        let message = message0 + &" ".repeat(m0len);
        queue!(
            io::stdout(),
            cursor::MoveTo(0, self.start_y),
            style::PrintStyledContent(
                style::style(&message)
                    .with(self.colors.normal.0)
                    .on(self.colors.normal.1)
            )
        )
        .unwrap();
    }

    pub fn resize(&mut self, total_rows: u16, total_cols: u16) {
        self.total_rows = total_rows;
        self.total_cols = total_cols;

        self.redraw();
    }
}
