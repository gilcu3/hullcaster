use std::cmp::min;
use std::collections::hash_map::Entry;

use crossterm::style::{self, Stylize};

use super::{Move, Panel, Scroll};
use crate::types::*;

/// Generic struct holding details about a list menu. These menus are
/// contained by the UI, and hold the list of podcasts or podcast
/// episodes. They also hold the Panel used to draw all elements to the
/// screen.
///
/// * `header` is an optional String of text that is printed above the
///   menu; the scrollable menu effectively starts below the header.
/// * `start_row` indicates the first row that is used for the menu;
///   this will be 0 if there is no header; otherwise, `start_row` will
///   be the first row below the header. Calculated relative to the
///   panel, i.e., a value between 0 and (n_row - 1)
/// * `top_row` indicates the top line of text that is shown on screen
///   (since the list of items can be longer than the available size of
///   the screen). `top_row` is calculated relative to the `items` index,
///   i.e., it will be a value between 0 and items.len()
/// * `selected` indicates which item on screen is currently highlighted.
///   It is calculated relative to the panel, i.e., a value between
///   0 and (n_row - 1)
/// * `active` indicates whether the menu is currently interactive, e.g.,
///   if the user scrolls up or down, this is the menu that will receive
///   those events.
#[derive(Debug)]
pub struct Menu<T>
where
    T: Clone + Menuable,
{
    pub panel: Panel,
    pub header: Option<String>,
    pub items: LockVec<T>,
    pub start_row: u16, // beginning of first row of menu
    pub top_row: u16,   // top row of text shown in window
    pub selected: u16,  // which line of text is highlighted
    pub active: bool,
    pub visible: bool,
}

impl<T: Clone + Menuable> Menu<T> {
    /// Creates a new menu.
    pub fn new(panel: Panel, header: Option<String>, items: LockVec<T>) -> Self {
        Self {
            panel,
            header,
            items,
            start_row: 0,
            top_row: 0,
            selected: 0,
            active: false,
            visible: false,
        }
    }

    /// Clears the terminal, and then prints the list of visible items
    /// to the terminal.
    pub fn redraw(&mut self) {
        if self.visible {
            self.panel.redraw();
            self.update_items();
        }
    }

    /// Prints the list of visible items to the terminal.
    pub fn update_items(&mut self) {
        self.start_row = self.print_header();
        if self.selected < self.start_row {
            self.selected = self.start_row;
        }

        let (map, _u, order) = self.items.borrow();
        if !order.is_empty() {
            // update selected item if list has gotten shorter
            let current_selected = self.get_menu_idx(self.selected);
            let list_len = order.len();
            if current_selected >= list_len {
                if list_len > 0 {
                    self.selected =
                        (self.selected as usize - (current_selected - list_len) - 1) as u16;
                } else {
                    self.selected = 0;
                }
            }

            // for visible rows, print strings from list
            for i in self.start_row..self.panel.get_rows() {
                if let Some(elem_id) = order.get(self.get_menu_idx(i)) {
                    let elem = map.get(elem_id).expect("Could not retrieve menu item.");

                    if i == self.selected || !elem.is_played() {
                        let style = if !elem.is_played() && i == self.selected {
                            style::ContentStyle::new()
                                .with(self.panel.colors.bold.0)
                                .on(self.panel.colors.bold.1)
                                .underline(self.panel.colors.bold.1)
                                .attribute(style::Attribute::Underlined)
                        } else if !elem.is_played() {
                            style::ContentStyle::new()
                                .with(self.panel.colors.bold.0)
                                .on(self.panel.colors.bold.1)
                                .attribute(style::Attribute::Bold)
                        } else {
                            style::ContentStyle::new()
                                .with(self.panel.colors.normal.0)
                                .on(self.panel.colors.normal.1)
                        };
                        self.panel.write_line(
                            i,
                            elem.get_title(self.panel.get_cols() as usize),
                            Some(style),
                        );
                    } else {
                        self.panel.write_line(
                            i,
                            elem.get_title(self.panel.get_cols() as usize),
                            None,
                        );
                    }
                } else {
                    break;
                }
            }
        }
    }

    /// If a header exists, prints lines of text to the panel to appear
    /// above the menu.
    fn print_header(&mut self) -> u16 {
        if let Some(header) = &self.header {
            self.panel.write_wrap_line(0, header, None) + 2
        } else {
            0
        }
    }

    /// Scrolls the menu up or down by `lines` lines.
    ///
    /// This function examines the new selected value, ensures it does
    /// not fall out of bounds, and then updates the panel to
    /// represent the new visible list.
    pub fn scroll(&mut self, lines: Scroll) -> bool {
        let list_len = self.items.len(true) as u16;
        if list_len == 0 {
            return false;
        }

        match lines {
            Scroll::Up(v) => {
                let selected_adj = self.selected - self.start_row;
                if selected_adj == 0 && self.start_row == self.top_row {
                    return false;
                } else if v <= selected_adj {
                    self.unhighlight_item(self.selected);
                    self.selected -= v;
                } else {
                    let list_scroll_amount = v - selected_adj;
                    if let Some(top) = self.top_row.checked_sub(list_scroll_amount) {
                        self.top_row = top;
                    } else {
                        self.top_row = 0;
                    }
                    self.selected = self.start_row;
                    self.panel.clear_inner();
                    self.update_items();
                }
                self.highlight_item(self.selected, self.active);
            }
            Scroll::Down(v) => {
                if self.get_menu_idx(self.selected) >= list_len as usize - 1 {
                    // we're at the bottom of the list
                    return false;
                }

                let n_row = self.panel.get_rows();
                let select_max = if list_len < n_row - self.start_row {
                    self.start_row + list_len - 1
                } else {
                    n_row - 1
                };

                if v <= (select_max - self.selected) {
                    self.unhighlight_item(self.selected);
                    self.selected += v;
                } else {
                    let list_scroll_amount = v - (n_row - self.selected - 1);
                    let visible_rows = n_row - self.start_row;
                    // can't scroll list if list is shorter than full screen
                    if list_len > visible_rows {
                        self.top_row =
                            min(self.top_row + list_scroll_amount, list_len - visible_rows);
                    }
                    self.selected = select_max;
                    self.panel.clear_inner();
                    self.update_items();
                }
                self.highlight_item(self.selected, self.active);
            }
        }
        true
    }

    pub fn move_item(&mut self, dir: Move) -> bool {
        let list_len = self.items.len(false) as u16;
        if list_len <= 1 {
            return false;
        }
        let selected = self.get_menu_idx(self.selected);
        match dir {
            Move::Up => {
                if selected == 0 {
                    return false;
                } else {
                    {
                        let mut order_vec = self.items.borrow_filtered_order();
                        order_vec.swap(selected, selected - 1);
                    }

                    if self.selected == 0 {
                        self.scroll(Scroll::Up(1));
                    } else {
                        self.selected -= 1;
                    }
                }
            }
            Move::Down => {
                if selected == (list_len - 1) as usize {
                    return false;
                } else {
                    {
                        let mut order_vec = self.items.borrow_filtered_order();
                        order_vec.swap(selected, selected + 1);
                    }

                    if self.selected == self.panel.get_rows() {
                        self.scroll(Scroll::Down(1));
                    } else {
                        self.selected += 1;
                    }
                }
            }
        }
        self.redraw();
        self.highlight_selected();
        true
    }

    /// Highlights the item in the menu, given a y-value.
    pub fn highlight_item(&mut self, item_y: u16, active: bool) {
        // if list is empty, will return None
        let el_details = self
            .items
            .map_single_by_index(self.get_menu_idx(item_y), |el| {
                (el.get_title(self.panel.get_cols() as usize), el.is_played())
            });

        if let Some((title, is_played)) = el_details {
            let mut style = style::ContentStyle::new();
            if active {
                style = style.with(self.panel.colors.highlighted_active.0).on(self
                    .panel
                    .colors
                    .highlighted_active
                    .1);
            } else {
                style =
                    style
                        .with(self.panel.colors.highlighted.0)
                        .on(self.panel.colors.highlighted.1);
            }
            style = if is_played {
                style.attribute(style::Attribute::NormalIntensity)
            } else {
                style.attribute(style::Attribute::Underlined)
            };
            self.panel.write_line(item_y, title, Some(style));
        }
    }

    /// Removes highlight on the item in the menu, given a y-value.
    pub fn unhighlight_item(&mut self, item_y: u16) {
        // if list is empty, will return None
        let el_details = self
            .items
            .map_single_by_index(self.get_menu_idx(item_y), |el| {
                (el.get_title(self.panel.get_cols() as usize), el.is_played())
            });

        if let Some((title, is_played)) = el_details {
            let style = if is_played {
                style::ContentStyle::new()
                    .with(self.panel.colors.normal.0)
                    .on(self.panel.colors.normal.1)
            } else {
                style::ContentStyle::new()
                    .with(self.panel.colors.bold.0)
                    .on(self.panel.colors.bold.1)
                    .attribute(style::Attribute::Bold)
            };
            self.panel.write_line(item_y, title, Some(style));
        }
    }

    /// Highlights the currently selected item in the menu, based on
    /// whether the menu is currently active or not.
    pub fn highlight_selected(&mut self) {
        self.highlight_item(self.selected, self.active);
    }

    /// Controls how the window changes when it is active (i.e.,
    /// available for user input to modify state).
    pub fn activate(&mut self) {
        self.active = true;
        self.panel.active = true;
    }

    /// Updates window size.
    pub fn resize(&mut self, n_row: u16, n_col: u16, start_x: u16) {
        self.panel.resize(n_row, n_col, start_x);
        let n_row = self.panel.get_rows();

        // if resizing moves selected item off screen, scroll the list
        // upwards to keep same item selected
        if self.selected > (n_row - 1) {
            self.top_row = self.top_row + self.selected - (n_row - 1);
            self.selected = n_row - 1;
        }
        self.redraw();
    }

    /// Given a row on the panel, this translates it into the
    /// corresponding menu item it represents. Note that this does not
    /// do any checks to ensure `screen_y` is between 0 and `n_rows`,
    /// or that the resulting menu index is between 0 and `n_items`.
    /// It's merely a straight translation.
    pub fn get_menu_idx(&self, screen_y: u16) -> usize {
        (self.top_row + screen_y - self.start_row) as usize
    }

    /// Controls how the window changes when it is inactive (i.e., not
    /// available for user input to modify state).
    pub fn deactivate(&mut self, keep_highlighted: bool) {
        self.active = false;
        self.panel.active = false;
        if keep_highlighted {
            self.highlight_item(self.selected, false);
        } else {
            self.unhighlight_item(self.selected);
        }
    }
}

impl Menu<Podcast> {
    /// Returns a cloned reference to the list of episodes from the
    /// currently selected podcast.
    pub fn get_episodes(&self) -> LockVec<Episode> {
        let index = self.get_menu_idx(self.selected);
        let (borrowed_map, _u, borrowed_order) = self.items.borrow();
        if borrowed_order.len() <= index {
            LockVec::new(Vec::new())
        } else {
            let pod_id = borrowed_order
                .get(index)
                .expect("Could not retrieve podcast.");
            borrowed_map
                .get(pod_id)
                .expect("Could not retrieve podcast info.")
                .episodes
                .clone()
        }
    }
}

impl Menu<NewEpisode> {
    /// Changes the status of the currently highlighted episode -- if it
    /// was selected to be downloaded, it will be unselected, and vice
    /// versa.
    pub fn select_item(&mut self) {
        let changed = self.change_item_selections(vec![self.get_menu_idx(self.selected)], None);
        if changed {
            self.update_items();
            self.highlight_selected();
        }
    }

    /// Changes the status of all items in the list. If there are any
    /// unselected episodes, this will convert all episodes to be
    /// selected; if all are selected already, only then will it convert
    /// all to unselected.
    pub fn select_all_items(&mut self) {
        let all_selected = self.items.map(|ep| ep.selected, false).iter().all(|x| *x);
        let changed =
            self.change_item_selections((0..self.items.len(false)).collect(), Some(!all_selected));
        if changed {
            self.update_items();
            self.highlight_selected();
        }
    }

    /// Given a list of index values in the menu, this changes the status
    /// of these episode -- if they were selected to be downloaded, they
    /// will be unselected, and vice versa. If `selection` is a boolean,
    /// however, it will be set to this value explicitly rather than just
    /// being reversed.
    fn change_item_selections(&mut self, indexes: Vec<usize>, selection: Option<bool>) -> bool {
        let mut changed = false;
        {
            let (mut borrowed_map, borrowed_order, _u) = self.items.borrow();
            for idx in indexes {
                if let Some(ep_id) = borrowed_order.get(idx) {
                    if let Entry::Occupied(mut ep) = borrowed_map.entry(*ep_id) {
                        let ep = ep.get_mut();
                        match selection {
                            Some(sel) => ep.selected = sel,
                            None => ep.selected = !ep.selected,
                        }
                        changed = true;
                    }
                }
            }
        }
        changed
    }
}

// TESTS ----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::rc::Rc;

    fn create_menu(n_row: u16, n_col: u16, top_row: u16, selected: u16) -> Menu<Episode> {
        let colors = Rc::new(crate::ui::AppColors::default());
        let titles = [
            "A Very Cool Episode",
            "This is a very long episode title but we'll get through it together",
            "An episode with le Unicodé",
            "How does an episode with emoji sound? 😉",
            "Here's another title",
            "Un titre, c'est moi!",
            "One more just for good measure",
        ];
        let mut items = Vec::new();
        for (i, t) in titles.iter().enumerate() {
            let played = i % 2 == 0;
            items.push(Episode {
                id: i as _,
                pod_id: 1,
                title: t.to_string(),
                url: String::new(),
                guid: String::new(),
                description: String::new(),
                pubdate: Some(Utc::now()),
                duration: Some(12345),
                path: None,
                played,
            });
        }

        let panel = Panel::new(
            "Episodes".to_string(),
            1,
            colors.clone(),
            n_row,
            n_col,
            0,
            (0, 0, 0, 0),
        );
        Menu {
            panel,
            header: None,
            items: LockVec::new(items),
            start_row: 0,
            top_row,
            selected,
            active: true,
            visible: true,
        }
    }

    #[test]
    fn scroll_up() {
        let real_rows = 5;
        let real_cols = 65;
        let mut menu = create_menu(real_rows + 2, real_cols + 3, 2, 0);
        menu.update_items();

        menu.scroll(Scroll::Up(1));

        let expected_top = menu
            .items
            .map_single_by_index(1, |ep| ep.get_title(real_cols as usize))
            .unwrap();
        let expected_bot = menu
            .items
            .map_single_by_index(5, |ep| ep.get_title(real_cols as usize))
            .unwrap();
        assert_eq!(menu.panel.get_row(0), expected_top);
        assert_eq!(menu.panel.get_row(4), expected_bot);
    }

    #[test]
    fn scroll_down() {
        let real_rows = 5;
        let real_cols = 65;
        let mut menu = create_menu(real_rows + 2, real_cols + 3, 0, 4);
        menu.update_items();

        menu.scroll(Scroll::Down(1));

        let expected_top = menu
            .items
            .map_single_by_index(1, |ep| ep.get_title(real_cols as usize))
            .unwrap();
        let expected_bot = menu
            .items
            .map_single_by_index(5, |ep| ep.get_title(real_cols as usize))
            .unwrap();

        assert_eq!(menu.panel.get_row(0), expected_top);
        assert_eq!(menu.panel.get_row(4), expected_bot);
    }

    #[test]
    fn resize_bigger() {
        let real_rows = 5;
        let real_cols = 65;
        let mut menu = create_menu(real_rows + 2, real_cols + 3, 0, 4);
        menu.update_items();

        menu.resize(real_rows + 2 + 5, real_cols + 3 + 5, 0);
        menu.update_items();

        assert_eq!(menu.top_row, 0);
        assert_eq!(menu.selected, 4);

        let non_empty: Vec<String> = menu
            .panel
            .buffer
            .iter()
            .filter_map(|x| if x.is_empty() { None } else { Some(x.clone()) })
            .collect();
        assert_eq!(non_empty.len(), menu.items.len(true));
    }

    #[test]
    fn resize_smaller() {
        let real_rows = 7;
        let real_cols = 65;
        let mut menu = create_menu(real_rows + 2, real_cols + 3, 0, 6);
        menu.update_items();

        menu.resize(real_rows + 2 - 2, real_cols + 3 - 5, 0);
        menu.update_items();

        assert_eq!(menu.top_row, 2);
        assert_eq!(menu.selected, 4);

        let non_empty: Vec<String> = menu
            .panel
            .buffer
            .iter()
            .filter_map(|x| if x.is_empty() { None } else { Some(x.clone()) })
            .collect();
        assert_eq!(non_empty.len(), (real_rows - 2) as usize);
    }

    #[test]
    fn chop_accent() {
        let real_rows = 5;
        let real_cols = 25;
        let mut menu = create_menu(real_rows + 2, real_cols + 5, 0, 0);
        menu.update_items();

        let expected = " An episode with le Unicod ".to_string();

        assert_eq!(menu.panel.get_row(2), expected);
    }

    #[test]
    fn chop_emoji() {
        let real_rows = 5;
        let real_cols = 38;
        let mut menu = create_menu(real_rows + 2, real_cols + 5, 0, 0);
        menu.update_items();

        let expected = " How does an episode with emoji sound?  ".to_string();

        assert_eq!(menu.panel.get_row(3), expected);
    }
}
