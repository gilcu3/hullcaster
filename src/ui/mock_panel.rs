use std::{cmp::Ordering, rc::Rc};

use crossterm::style;

use super::AppColors;

/// Struct holding the raw data used for building the details panel.
#[derive(Debug)]
pub struct Panel {
    pub buffer: Vec<String>,
    pub screen_pos: usize,
    pub colors: Rc<AppColors>,
    pub title: String,
    pub start_x: u16,
    pub n_row: u16,
    pub n_col: u16,
    pub margins: (u16, u16, u16, u16),
    pub active: bool,
}

impl Panel {
    pub fn new(
        title: String, screen_pos: usize, colors: Rc<AppColors>, n_row: u16, n_col: u16,
        start_x: u16, margins: (u16, u16, u16, u16),
    ) -> Self {
        // we represent the window as a vector of Strings instead of
        // printing to the terminal buffer
        let buffer = vec![String::new(); (n_row - 2) as usize];

        Panel {
            buffer,
            screen_pos,
            colors,
            title,
            start_x,
            n_row,
            n_col,
            margins,
            active: false,
        }
    }

    pub fn redraw(&self) {}

    pub fn clear_inner(&mut self) {
        self.buffer = vec![String::new(); (self.n_row - 2) as usize];
    }

    pub fn write_line(&mut self, y: u16, string: String, _style: Option<style::ContentStyle>) {
        self.buffer[y as usize] = string;
    }

    pub fn write_key_value_line(
        &mut self, y: u16, key: String, value: String, _key_style: Option<style::ContentStyle>,
        _value_style: Option<style::ContentStyle>,
    ) {
        self.buffer[y as usize] = format!("{key}: {value}");
    }

    pub fn write_wrap_line(
        &mut self, start_y: u16, string: &str, _style: Option<style::ContentStyle>,
    ) -> u16 {
        let mut row = start_y;
        let max_row = self.get_rows();
        let wrapper = textwrap::wrap(string, self.get_cols() as usize);
        for line in wrapper {
            self.write_line(row, line.to_string(), None);
            row += 1;

            if row >= max_row {
                break;
            }
        }
        row - 1
    }

    pub fn resize(&mut self, n_row: u16, n_col: u16, start_x: u16) {
        self.n_row = n_row;
        self.n_col = n_col;
        self.start_x = start_x;

        let new_len = (n_row - 2) as usize;
        let len = self.buffer.len();
        match new_len.cmp(&len) {
            Ordering::Greater => {
                for _ in (new_len - len)..new_len {
                    self.buffer.push(String::new());
                }
            }
            Ordering::Less => self.buffer.truncate(new_len),
            Ordering::Equal => {}
        }
    }

    pub fn get_rows(&self) -> u16 {
        // 2 for border on top and bottom
        self.n_row - self.margins.0 - self.margins.2 - 2
    }

    pub fn get_cols(&self) -> u16 {
        // 2 for border, and 1 extra for some reason...
        self.n_col - self.margins.1 - self.margins.3 - 3
    }

    pub fn get_row(&self, row: usize) -> String {
        self.buffer[row].clone()
    }
}
