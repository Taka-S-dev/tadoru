use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::actions::{self, Action, RunMode};

pub enum Decision {
    Stay,
    Close,
    Run(Box<Action>),
}

pub struct Menu {
    pub target: PathBuf,
    items: Vec<Action>,
    query: String,
    visible: Vec<usize>,
    selected: usize,
    error: Option<String>,
    /// Shown when no actions.json exists. The command that writes one is the
    /// only way to learn the format, and nothing else on this screen says it.
    hint: Option<&'static str>,
    /// Commands that act on tadoru rather than on the selected item.
    tools: Vec<Action>,
    /// Set by a click in that zone, consumed by the Enter that follows it.
    pending_tool: Option<usize>,
    mouse_rows: Rect,
    mouse_tools: Rect,
    mouse_first: usize,
    /// The menu opens on its keys and only takes text once asked, so a single
    /// letter runs an action instead of needing a modifier held with it.
    filtering: bool,
}

impl Menu {
    pub fn new(target: PathBuf) -> Self {
        let (items, error) = actions::load(&target);
        let hint = (error.is_none() && actions::config_path().is_ok_and(|path| !path.exists()))
            .then_some("No actions.json yet. Run tadoru actions init to add your own.");
        let visible = (0..items.len()).collect();
        Self {
            target,
            items,
            query: String::new(),
            visible,
            selected: 0,
            error,
            hint,
            tools: actions::tools(),
            pending_tool: None,
            mouse_rows: Rect::default(),
            mouse_tools: Rect::default(),
            mouse_first: 0,
            filtering: false,
        }
    }

    /// Which action a letter runs. A setting that claims a letter takes it from
    /// the built-in action that had it, so one key never runs two things.
    fn owner(&self, ch: char) -> Option<usize> {
        let ch = ch.to_ascii_lowercase();
        self.items
            .iter()
            .rposition(|action| action.key() == Some(ch))
    }

    /// The letter to print beside a row, which is only the row that wins it.
    fn shown_key(&self, index: usize) -> Option<char> {
        let ch = self.items[index].key()?;
        (self.owner(ch) == Some(index)).then_some(ch)
    }

    /// A tool keeps its letter only while nothing in the list has taken it, so
    /// a setting still wins the letter it asked for.
    fn tool_owner(&self, ch: char) -> Option<usize> {
        let ch = ch.to_ascii_lowercase();
        self.owner(ch)
            .is_none()
            .then(|| self.tools.iter().position(|tool| tool.key() == Some(ch)))
            .flatten()
    }

    fn run(&self, index: usize) -> Decision {
        Decision::Run(Box::new(self.items[index].clone()))
    }

    fn run_tool(&self, index: usize) -> Decision {
        Decision::Run(Box::new(self.tools[index].clone()))
    }

    pub fn handle(&mut self, key: KeyEvent) -> Decision {
        if let Some(index) = self.pending_tool.take()
            && key.code == KeyCode::Enter
        {
            return self.run_tool(index);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Holding Alt runs an action from either mode, so a key learned from
        // the list still works with the filter focused and a name half typed.
        if key.modifiers.contains(KeyModifiers::ALT) && !ctrl {
            if let KeyCode::Char(ch) = key.code {
                return self.press(ch).unwrap_or(Decision::Stay);
            }
            return Decision::Stay;
        }
        match (key.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c' | 'p'), true) => return Decision::Close,
            (KeyCode::Enter, _) => {
                if let Some(&index) = self.visible.get(self.selected) {
                    return Decision::Run(Box::new(self.items[index].clone()));
                }
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => {
                self.selected = self.selected.saturating_sub(1)
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => {
                self.selected = (self.selected + 1).min(self.visible.len().saturating_sub(1))
            }
            (KeyCode::Backspace, _) => {
                self.query.pop();
                self.filter();
            }
            (KeyCode::Char('u'), true) => {
                self.query.clear();
                self.filter();
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => self.set_filtering(!self.filtering),
            (KeyCode::Char(ch), false) if crate::keys::is_typed_text(&key) => {
                if self.filtering {
                    self.query.push(ch);
                    self.filter();
                } else if let Some(decision) = self.press(ch) {
                    return decision;
                } else if ch == '/' {
                    // The usual key for starting a search, and one no action
                    // can claim, so it is free to mean this here.
                    self.set_filtering(true);
                }
            }
            _ => {}
        }
        Decision::Stay
    }

    /// The action a letter runs, from the list first so that a setting keeps
    /// the letter it asked for even when a tool already prints it.
    fn press(&self, ch: char) -> Option<Decision> {
        if let Some(index) = self.owner(ch) {
            return Some(self.run(index));
        }
        self.tool_owner(ch).map(|index| self.run_tool(index))
    }

    /// Leaving the filter drops what was typed, so the list a key acts on is
    /// the whole list again rather than yesterday's narrowing.
    fn set_filtering(&mut self, on: bool) {
        self.filtering = on;
        if !on && !self.query.is_empty() {
            self.query.clear();
            self.filter();
        }
    }

    fn filter(&mut self) {
        self.selected = 0;
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buffer = Vec::new();
        let mut matches: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, action)| {
                pattern
                    .score(Utf32Str::new(action.name(), &mut buffer), &mut matcher)
                    .map(|score| (score, index))
            })
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.visible = matches.into_iter().map(|(_, index)| index).collect();
    }

    /// Select a displayed action; true requests execution through the Enter path.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        // The zone sits outside the list, so a click there cannot move a
        // selection. It is remembered instead and read by the Enter that the
        // caller sends straight after a click it accepted.
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.mouse_tools.contains((mouse.column, mouse.row).into())
        {
            self.pending_tool = Some(0);
            return true;
        }
        if !self.mouse_rows.contains((mouse.column, mouse.row).into()) {
            return false;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let row = self.mouse_first + (mouse.row - self.mouse_rows.y) as usize;
                if row < self.visible.len() {
                    self.selected = row;
                    return true;
                }
            }
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(3),
            MouseEventKind::ScrollDown => {
                self.selected = self
                    .selected
                    .saturating_add(3)
                    .min(self.visible.len().saturating_sub(1))
            }
            _ => {}
        }
        false
    }

    pub fn render(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        self.mouse_rows = Rect::default();
        self.mouse_tools = Rect::default();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Actions ")
            .title_bottom(if self.filtering {
                " Click / Enter: run  Tab: back to keys  Esc / Ctrl-P: close "
            } else {
                " Click / Enter: run  Tab: filter  Esc / Ctrl-P: close "
            });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let head = match (&self.error, &self.hint) {
            (Some(_), _) => 3,
            (None, Some(_)) => 2,
            _ => 0,
        };
        // A rule above the zone, so it reads as separate from the list rather
        // than as its last row. It follows the list instead of sitting at the
        // foot of the window, which in a full-height menu put it far enough
        // below the actions to be missed. Dropped when there is no room.
        let available = inner.height.saturating_sub(2 + head);
        let tools_height = if self.tools.is_empty() || available < 3 {
            0
        } else {
            2
        };
        let list_height = (self.visible.len() as u16)
            .max(1)
            .min(available.saturating_sub(tools_height));
        let [target, prompt, warning, rows, tools, _rest] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(head),
            Constraint::Length(list_height),
            Constraint::Length(tools_height),
            Constraint::Min(0),
        ])
        .areas(inner);
        if tools_height > 0 {
            frame.render_widget(
                Paragraph::new("─".repeat(tools.width as usize))
                    .style(Style::default().fg(Color::Indexed(238))),
                Rect { height: 1, ..tools },
            );
            let line = Rect {
                y: tools.y + 1,
                height: 1,
                ..tools
            };
            let key = match self.tools[0]
                .key()
                .filter(|&ch| self.tool_owner(ch).is_some())
            {
                Some(ch) => format!("{ch}  "),
                None => "   ".to_string(),
            };
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw("  "),
                    Span::raw(key),
                    Span::raw(self.tools[0].name().to_string()),
                ]))
                .style(Style::default().fg(Color::DarkGray)),
                line,
            );
            self.mouse_tools = line;
        }
        frame.render_widget(
            Paragraph::new(self.target.display().to_string())
                .style(Style::default().fg(Color::Cyan)),
            target,
        );
        if self.filtering {
            frame.render_widget(Paragraph::new(format!("> {}", self.query)), prompt);
            if prompt.width > 0 && prompt.height > 0 {
                frame.set_cursor_position((
                    prompt.x + (2 + self.query.width() as u16).min(prompt.width - 1),
                    prompt.y,
                ));
            }
        } else {
            // Say which keys the list is listening for. Without this the rows
            // look like plain labels and the letters beside them like noise.
            frame.render_widget(
                Paragraph::new("Press a key to run.  Tab: filter")
                    .style(Style::default().fg(Color::DarkGray)),
                prompt,
            );
        }
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(error.as_str())
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                warning,
            );
        } else if let Some(hint) = self.hint {
            frame.render_widget(
                Paragraph::new(hint)
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: false }),
                warning,
            );
        }
        if self.visible.is_empty() {
            frame.render_widget(Paragraph::new("No matching actions"), rows);
            return;
        }
        let first = self
            .selected
            .saturating_sub((rows.height as usize).saturating_sub(1));
        self.mouse_rows = rows;
        self.mouse_first = first;
        let shown: Vec<(usize, usize)> = self
            .visible
            .iter()
            .enumerate()
            .skip(first)
            .take(rows.height as usize)
            .map(|(row, &index)| (row, index))
            .collect();
        // The keys go in a column of their own on the left, so they can be read
        // down the list. Nothing is indented when no visible action has one.
        let keyed = shown
            .iter()
            .any(|&(_, index)| self.shown_key(index).is_some());
        let (label, width): (fn(char) -> String, usize) = if self.filtering {
            (|ch| format!("alt+{ch}  "), 7)
        } else {
            (|ch| format!("{ch}  "), 3)
        };
        let items: Vec<ListItem> = shown
            .iter()
            .map(|&(row, index)| {
                let action = &self.items[index];
                let suffix = if action.run_mode() == RunMode::Terminal {
                    "  [terminal]"
                } else {
                    ""
                };
                let key = match (keyed, self.shown_key(index)) {
                    (true, Some(ch)) => label(ch),
                    (true, None) => " ".repeat(width),
                    (false, _) => String::new(),
                };
                let line = Line::from(vec![
                    Span::raw(if row == self.selected { "▌ " } else { "  " }),
                    Span::styled(key, Style::default().fg(Color::DarkGray)),
                    Span::raw(action.name().to_string()),
                    Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                ]);
                let style = if row == self.selected {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Indexed(236))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(line).style(style)
            })
            .collect();
        frame.render_widget(List::new(items), rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn menu_of(items: Vec<Action>) -> Menu {
        let visible = (0..items.len()).collect();
        Menu {
            target: PathBuf::from("selected file.txt"),
            items,
            query: String::new(),
            visible,
            selected: 0,
            error: None,
            hint: None,
            tools: actions::tools(),
            pending_tool: None,
            mouse_rows: Rect::default(),
            mouse_tools: Rect::default(),
            mouse_first: 0,
            filtering: false,
        }
    }

    #[test]
    fn a_tall_window_keeps_the_zone_next_to_the_list() {
        let mut menu = menu_of(vec![Action::Reveal, Action::Editor, Action::Copy]);
        let mut terminal = Terminal::new(TestBackend::new(46, 26)).unwrap();
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (1..45)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        // Three actions on rows 3 to 5, then the zone. Pinning it to the foot
        // of the window instead left eighteen blank rows between the two.
        assert_eq!(row(5), "  c  Copy path");
        assert_eq!(row(6), "─".repeat(44));
        assert_eq!(row(7), "  t  Open temporary copies folder");
        assert_eq!(row(8), "");
    }

    #[test]
    fn the_copies_folder_sits_apart_from_the_actions_on_the_selection() {
        let mut menu = menu_of(vec![Action::Reveal, Action::Copy]);
        let mut terminal = Terminal::new(TestBackend::new(46, 12)).unwrap();
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
            let buffer = terminal.backend().buffer();
            (1..45)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        }
        // A blank line keeps it from reading as the last row of the list.
        assert_eq!(row(&terminal, 5), "─".repeat(44));
        assert_eq!(row(&terminal, 6), "  t  Open temporary copies folder");

        // The filter never hides it, because it is not one of the items.
        menu.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for ch in "zzz".chars() {
            menu.handle(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(menu.visible.is_empty());
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        // With nothing matching, the list keeps one row for its message and
        // the zone stays right under it.
        assert_eq!(row(&terminal, 4), "─".repeat(44));
        assert_eq!(row(&terminal, 5), "  t  Open temporary copies folder");

        // Its key runs it from either mode, and a click on it does too.
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT)),
            Decision::Run(action) if matches!(*action, Action::TempFolder)
        ));
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: menu.mouse_tools.x,
            row: menu.mouse_tools.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(menu.handle_mouse(click));
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Decision::Run(action) if matches!(*action, Action::TempFolder)
        ));
        // A click elsewhere afterwards must not run it a second time.
        assert!(!matches!(
            menu.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Decision::Run(action) if matches!(*action, Action::TempFolder)
        ));
    }

    #[test]
    fn a_menu_with_no_config_says_how_to_start_one() {
        let mut menu = menu_of(vec![Action::Reveal]);
        menu.hint = Some("No actions.json yet. Run tadoru actions init to add your own.");
        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("tadoru actions init"), "{screen}");
        // The list still starts below it rather than being pushed off screen.
        assert!(screen.contains("Open in file manager"), "{screen}");

        menu.hint = None;
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(!screen.contains("actions init"), "{screen}");
    }

    #[test]
    fn the_menu_starts_on_its_keys_and_only_types_once_asked() {
        let mut menu = menu_of(vec![Action::Reveal, Action::Copy]);
        assert!(!menu.filtering);
        // A bare letter runs its action, which is the whole point of opening
        // on the keys rather than on a text box.
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Decision::Run(action) if matches!(*action, Action::Copy)
        ));
        assert!(menu.query.is_empty());
        // A letter no action claims must not type, or the mode would be a lie.
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)),
            Decision::Stay
        ));
        assert!(menu.query.is_empty());

        for enter in [KeyCode::Tab, KeyCode::Char('/')] {
            menu.handle(KeyEvent::new(enter, KeyModifiers::NONE));
            assert!(menu.filtering, "{enter:?}");
            for ch in "copy".chars() {
                menu.handle(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            assert_eq!(menu.query, "copy", "{enter:?}");
            assert_eq!(menu.visible, [1], "{enter:?}");
            // Leaving drops the text, so the next key acts on the whole list.
            menu.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            assert!(!menu.filtering, "{enter:?}");
            assert!(menu.query.is_empty(), "{enter:?}");
            assert_eq!(menu.visible, [0, 1], "{enter:?}");
        }
    }

    #[test]
    fn an_alt_key_runs_its_action_even_while_the_filter_hides_it() {
        let mut menu = menu_of(vec![Action::Reveal, Action::Copy]);
        menu.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(menu.filtering);
        for ch in "reveal".chars() {
            menu.handle(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(
            !menu.visible.contains(&1),
            "copy path should be filtered out"
        );
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT)),
            Decision::Run(action) if matches!(*action, Action::Copy)
        ));
        // A letter nothing claims does nothing, rather than closing or typing.
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT)),
            Decision::Stay
        ));
        assert_eq!(menu.query, "reveal");
    }

    #[test]
    fn a_configured_key_takes_the_letter_from_the_built_in_action() {
        let custom = Action::Custom {
            definition: crate::actions::test_definition("Compare", Some("c")),
            config_dir: PathBuf::from("config"),
        };
        let menu = menu_of(vec![Action::Reveal, Action::Copy, custom]);
        assert_eq!(menu.owner('c'), Some(2));
        assert_eq!(menu.shown_key(2), Some('c'));
        // The built-in keeps its letter in the enum but must not advertise one
        // it no longer runs.
        assert_eq!(menu.shown_key(1), None);
        assert_eq!(menu.shown_key(0), Some('f'));

        let mut menu = menu;
        let mut terminal = Terminal::new(TestBackend::new(44, 10)).unwrap();
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        // Columns 0 and 43 are the border.
        let rows: Vec<String> = (3..6)
            .map(|y| {
                let buffer = terminal.backend().buffer();
                (1..43)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        // The names line up because the row without a key is padded to match.
        assert_eq!(rows[0], "▌ f  Open in file manager");
        assert_eq!(rows[1], "     Copy path");
        assert_eq!(rows[2], "  c  Compare");

        // With the filter focused the same keys need Alt, and say so.
        menu.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        terminal
            .draw(|frame| menu.render(frame.area(), frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (1..43)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        };
        assert_eq!(row(3), "▌ alt+f  Open in file manager");
        assert_eq!(row(2), ">");
    }

    #[test]
    fn alt_and_ctrl_chars_stay_out_of_the_filter() {
        // Started with the filter focused: this is about the modifiers, not
        // about which mode the menu opens in.
        let mut menu = Menu {
            target: PathBuf::from("selected file.txt"),
            items: vec![Action::Reveal, Action::Copy],
            query: String::new(),
            visible: vec![0, 1],
            selected: 0,
            error: None,
            hint: None,
            tools: actions::tools(),
            pending_tool: None,
            mouse_rows: Rect::default(),
            mouse_tools: Rect::default(),
            mouse_first: 0,
            filtering: true,
        };
        menu.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        menu.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(menu.query.is_empty());
        assert_eq!(menu.visible, [0, 1]);
        menu.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(menu.query, "d");
    }

    #[test]
    fn filtering_keeps_target_fixed_and_escape_only_closes_the_menu() {
        let target = PathBuf::from("selected file.txt");
        let mut menu = Menu {
            target: target.clone(),
            items: vec![Action::Reveal, Action::Copy],
            query: String::new(),
            visible: vec![0, 1],
            selected: 0,
            error: None,
            hint: None,
            tools: actions::tools(),
            pending_tool: None,
            mouse_rows: Rect::default(),
            mouse_tools: Rect::default(),
            mouse_first: 0,
            filtering: true,
        };
        for ch in "copy".chars() {
            menu.handle(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(menu.visible, [1]);
        assert_eq!(menu.target, target);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| menu.render(Rect::new(3, 4, 50, 12), frame))
            .unwrap();
        let click = |row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: menu.mouse_rows.x,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let valid = click(menu.mouse_rows.y);
        let blank = click(menu.mouse_tools.y + 3);
        let header = click(menu.mouse_rows.y - 1);
        assert!(!menu.handle_mouse(blank));
        assert!(!menu.handle_mouse(header));
        assert!(menu.handle_mouse(valid));
        assert!(
            matches!(menu.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), Decision::Run(action) if matches!(*action, Action::Copy))
        );
        menu.handle(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(menu.visible.is_empty());
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Decision::Stay
        ));
        for (width, height) in [(2, 2), (40, 12), (100, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| menu.render(frame.area(), frame))
                .unwrap();
        }
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Decision::Close
        ));
    }
}
