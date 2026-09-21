//! What the picker can do, one plain letter each, in a panel that opens on
//! Ctrl-Space.
//!
//! The key hints on the bottom border are cut to what fits, so on a narrow
//! terminal most keys do not appear in them, and `?` cannot ask for help
//! because it is typed into the filter. Here every key is listed beside the
//! shortcut that does the same without the panel, which is how the shortcuts
//! get learned. Only Ctrl-Space itself has to reach tadoru: a terminal that
//! keeps a shortcut for itself does not take the letter away as well.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem};
use unicode_width::UnicodeWidthStr;

use crate::Mode;
use crate::action_menu::{ARROW, ARROW_STYLE, KEY_STYLE, SELECTED_STYLE, float, open_panel};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Go(Mode),
    /// Anything the picker already has a shortcut for is run by pressing it,
    /// so the panel cannot drift from what the shortcut does.
    Press(KeyCode, KeyModifiers),
}

pub enum Decision {
    Stay,
    Close,
    Run(Command),
}

struct Entry {
    key: char,
    name: &'static str,
    /// The shortcut that does the same with the panel closed.
    shortcut: &'static str,
    command: Command,
}

pub struct KeyMenu {
    entries: Vec<Entry>,
    /// How many entries, from the top, are the tabs. A rule follows them.
    tabs: usize,
    /// The tab the picker is on, which is marked as it is on the top border.
    here: Mode,
    selected: usize,
    mouse_rows: Rect,
    mouse_panel: Rect,
}

impl KeyMenu {
    /// `origin` is the mode Right goes back to from recent and favorites,
    /// which the row for it names.
    pub fn new(here: Mode, origin: Mode) -> Self {
        let tab = |key, mode: Mode, shortcut| Entry {
            key,
            name: mode.label(),
            shortcut,
            command: Command::Go(mode),
        };
        let press = |key, name, shortcut, code, modifiers| Entry {
            key,
            name,
            shortcut,
            command: Command::Press(code, modifiers),
        };
        let browsing = here == Mode::Browse;
        let (none, ctrl) = (KeyModifiers::NONE, KeyModifiers::CONTROL);
        let entries = vec![
            tab('d', Mode::Dirs, "^D"),
            tab('f', Mode::Files, "^F"),
            tab('r', Mode::Recent, "^R"),
            tab('s', Mode::Favorites, "^S"),
            tab('b', Mode::Browse, "Tab"),
            press(
                'l',
                match (here, origin) {
                    (Mode::Browse, _) => "Go down a level",
                    // A list of places is left for where it was opened from,
                    // now at the place chosen.
                    (Mode::Recent | Mode::Favorites, Mode::Dirs) => "Search dirs from it",
                    (Mode::Recent | Mode::Favorites, Mode::Files) => "Search files from it",
                    (Mode::Recent | Mode::Favorites, _) => "Show it in browse",
                    _ => "Go into the selection",
                },
                "Right",
                KeyCode::Right,
                none,
            ),
            press(
                'h',
                if browsing {
                    "Go up a level"
                } else {
                    "Search one level up"
                },
                "Left",
                KeyCode::Left,
                none,
            ),
            press('[', "Back", "^Left", KeyCode::Left, ctrl),
            press(']', "Forward", "^Right", KeyCode::Right, ctrl),
            press(
                'a',
                "Actions for the selection",
                "^P",
                KeyCode::Char('p'),
                ctrl,
            ),
            press('p', "Pin or unpin", "^B", KeyCode::Char('b'), ctrl),
            press('o', "Show in file manager", "^O", KeyCode::Char('o'), ctrl),
            press(
                'e',
                "Open with default application",
                "^E",
                KeyCode::Char('e'),
                ctrl,
            ),
            press('5', "Refresh", "F5", KeyCode::F(5), none),
        ];
        // Opens on the tab the picker is on, so Down goes to the next one.
        let selected = entries
            .iter()
            .position(|entry| entry.command == Command::Go(here))
            .unwrap_or(0);
        Self {
            entries,
            tabs: 5,
            here,
            selected,
            mouse_rows: Rect::default(),
            mouse_panel: Rect::default(),
        }
    }

    pub fn handle(&mut self, key: KeyEvent) -> Decision {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            // Ctrl-Space closes what it opened.
            (KeyCode::Esc, _) | (KeyCode::Char('c' | ' '), true) | (KeyCode::Null, _) => {
                Decision::Close
            }
            (KeyCode::Enter, _) => Decision::Run(self.entries[self.selected].command),
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => {
                self.selected = self.selected.saturating_sub(1);
                Decision::Stay
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => {
                self.selected = (self.selected + 1).min(self.entries.len() - 1);
                Decision::Stay
            }
            (KeyCode::Char(ch), false) if crate::keys::is_typed_text(&key) => {
                let ch = ch.to_ascii_lowercase();
                match self.entries.iter().find(|entry| entry.key == ch) {
                    Some(entry) => Decision::Run(entry.command),
                    None => Decision::Stay,
                }
            }
            _ => Decision::Stay,
        }
    }

    /// The key a mouse event stands for: Enter on a row, which it selects
    /// first, and Esc anywhere outside the panel.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<KeyCode> {
        let position = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if self.mouse_rows.contains(position) => {
                let row = (mouse.row - self.mouse_rows.y) as usize;
                // The rule between the tabs and the rest is not a row.
                let index = match row.cmp(&self.tabs) {
                    std::cmp::Ordering::Less => row,
                    std::cmp::Ordering::Equal => return None,
                    std::cmp::Ordering::Greater => row - 1,
                };
                if index >= self.entries.len() {
                    return None;
                }
                self.selected = index;
                Some(KeyCode::Enter)
            }
            MouseEventKind::Down(_) if !self.mouse_panel.contains(position) => Some(KeyCode::Esc),
            MouseEventKind::ScrollUp => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            MouseEventKind::ScrollDown => {
                self.selected = (self.selected + 1).min(self.entries.len() - 1);
                None
            }
            _ => None,
        }
    }

    fn panel(&self, area: Rect) -> Rect {
        let widest = self
            .entries
            .iter()
            .map(|entry| entry.name.width() + 2 + entry.shortcut.width())
            .max()
            .unwrap_or(0);
        // Marker, key and arrow, the row, a space and the borders.
        let width = (2 + 1 + ARROW.width() + widest + 1 + 2) as u16;
        let height = self.entries.len() as u16 + 1 + 2;
        float(area, width, height)
    }

    pub fn render(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        let area = self.panel(area);
        self.mouse_panel = area;
        let inner = open_panel(area, frame, " Keys ", " Enter: run  Esc: close ");
        let [rows, _rest] = Layout::vertical([
            Constraint::Length(self.entries.len() as u16 + 1),
            Constraint::Min(0),
        ])
        .areas(inner);
        self.mouse_rows = rows;
        let shortcut_style = Style::default().fg(Color::DarkGray);
        let width = rows.width as usize;
        let mut items: Vec<ListItem> = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            if index == self.tabs {
                items.push(
                    ListItem::new("─".repeat(width))
                        .style(Style::default().fg(Color::Indexed(238))),
                );
            }
            let used = 2 + 1 + ARROW.width() + entry.name.width() + entry.shortcut.width();
            let name_style = if entry.command == Command::Go(self.here) {
                Style::default().add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::raw(if index == self.selected { "▌ " } else { "  " }),
                Span::styled(entry.key.to_string(), KEY_STYLE),
                Span::styled(ARROW, ARROW_STYLE),
                Span::styled(entry.name, name_style),
                // The shortcuts line up down the right edge.
                Span::raw(" ".repeat(width.saturating_sub(used + 1))),
                Span::styled(entry.shortcut, shortcut_style),
            ]);
            items.push(ListItem::new(line).style(if index == self.selected {
                SELECTED_STYLE
            } else {
                Style::default()
            }));
        }
        frame.render_widget(List::new(items), rows);
        // The picker underneath put the cursor in its own prompt, where typing
        // no longer goes while the panel is open.
        let selected_row = self.selected + usize::from(self.selected >= self.tabs);
        if rows.width > 0 && (selected_row as u16) < rows.height {
            frame.set_cursor_position((rows.x + 2, rows.y + selected_row as u16));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    #[test]
    fn a_plain_letter_runs_its_row_and_esc_or_ctrl_space_closes() {
        let mut menu = KeyMenu::new(Mode::Dirs, Mode::Dirs);
        assert!(matches!(
            menu.handle(key('s')),
            Decision::Run(Command::Go(Mode::Favorites))
        ));
        assert!(matches!(
            menu.handle(key('B')),
            Decision::Run(Command::Go(Mode::Browse))
        ));
        assert!(matches!(
            menu.handle(key('a')),
            Decision::Run(Command::Press(KeyCode::Char('p'), KeyModifiers::CONTROL))
        ));
        // A letter with no row does nothing rather than closing the panel.
        assert!(matches!(menu.handle(key('z')), Decision::Stay));
        for close in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
        ] {
            assert!(matches!(menu.handle(close), Decision::Close));
        }
    }

    #[test]
    fn it_opens_on_the_current_tab_and_enter_runs_the_selection() {
        let mut menu = KeyMenu::new(Mode::Recent, Mode::Browse);
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Decision::Run(Command::Go(Mode::Recent))
        ));
        menu.handle(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Decision::Run(Command::Go(Mode::Favorites))
        ));
        // Every key is its own, or a letter would run the wrong row.
        let mut keys: Vec<char> = menu.entries.iter().map(|entry| entry.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), menu.entries.len());
    }

    #[test]
    fn rows_name_the_shortcut_and_clicks_land_on_the_right_row() {
        let mut menu = KeyMenu::new(Mode::Browse, Mode::Browse);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = menu.mouse_rows;
        let text = |y: u16| -> String {
            (rows.x..rows.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(text(rows.y).starts_with("  d → dirs"), "{}", text(rows.y));
        assert!(text(rows.y).trim_end().ends_with("^D"));
        // Opened from browse, the two moves are named for what they do there.
        assert!(text(rows.y + 6).contains("l → Go down a level"));
        assert!(text(rows.y + 5).starts_with("─"));
        // From favorites the same row says where Right goes back to.
        let names = |here, origin| -> Vec<&str> {
            KeyMenu::new(here, origin)
                .entries
                .iter()
                .map(|entry| entry.name)
                .collect()
        };
        assert!(names(Mode::Favorites, Mode::Files).contains(&"Search files from it"));
        assert!(names(Mode::Favorites, Mode::Browse).contains(&"Show it in browse"));
        assert!(names(Mode::Dirs, Mode::Dirs).contains(&"Go into the selection"));

        let click = |row: u16, column: u16| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Below the rule the rows are one further down than their index.
        assert_eq!(
            menu.handle_mouse(click(rows.y + 6, rows.x + 3)),
            Some(KeyCode::Enter)
        );
        assert_eq!(menu.entries[menu.selected].key, 'l');
        assert_eq!(menu.handle_mouse(click(rows.y + 5, rows.x + 3)), None);
        // A click outside the panel closes it.
        assert_eq!(menu.handle_mouse(click(0, 0)), Some(KeyCode::Esc));
    }
}
