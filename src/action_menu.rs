use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::actions::{self, Action, RunMode, Then};

/// Joins a key to what it runs, as in `f → Open in file manager`.
pub(crate) const ARROW: &str = " → ";
/// The key is what gets pressed, so it is the bright part of a row, and the
/// arrow only joins it to the name.
pub(crate) const KEY_STYLE: Style = Style::new().fg(Color::Indexed(110));
pub(crate) const ARROW_STYLE: Style = Style::new().fg(Color::Indexed(240));
pub(crate) const SELECTED_STYLE: Style = Style::new()
    .fg(Color::White)
    .bg(Color::Indexed(236))
    .add_modifier(Modifier::BOLD);
/// The key column while the menu waits for a key: one character and the arrow.
const KEY_WIDTH: usize = 4;
/// The same column while filtering, where the keys need Alt: `alt+f → `.
const FILTER_KEY_WIDTH: usize = 8;
const TERMINAL_SUFFIX: &str = "  [terminal]";

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
    /// What follows an action run from here. Shown in the title, switched
    /// with Ctrl-X, and handed back to the picker for the rest of the run.
    pub after: Then,
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
            after: Then::Stay,
        }
    }

    /// Which action a letter runs. A setting that claims a letter takes it from
    /// the built-in action that had it, so one key never runs two things.
    pub fn then(mut self, after: Then) -> Self {
        self.after = after;
        self
    }

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
            (KeyCode::Backspace, _) | (KeyCode::Char('h'), true) => {
                self.query.pop();
                self.filter();
            }
            (KeyCode::Char('u'), true) => {
                self.query.clear();
                self.filter();
            }
            // Between staying and quitting; cd counts as quitting here, so
            // from it the switch goes to stay.
            (KeyCode::Char('x'), true) => {
                self.after = if self.after == Then::Stay {
                    Then::Quit
                } else {
                    Then::Stay
                };
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

    /// Lines above the list for an error in actions.json, or the pointer to
    /// the command that writes one.
    fn head(&self) -> u16 {
        match (&self.error, &self.hint) {
            (Some(_), _) => 3,
            (None, Some(_)) => 2,
            _ => 0,
        }
    }

    /// Where the menu floats: the bottom right of `area`, as wide as its
    /// longest row and as tall as its list.
    ///
    /// The right is where the preview is, so the list on the left, with the
    /// item the actions are about to run on, stays in view. Sized by every
    /// action rather than the ones a filter leaves, so typing does not make
    /// the panel jump. A screen too small to float in gets the whole area.
    fn panel(&self, area: Rect) -> Rect {
        const MIN_WIDTH: u16 = 48;
        let widest = self
            .items
            .iter()
            .chain(&self.tools)
            .map(|action| {
                action.name().width()
                    + if action.run_mode() == RunMode::Terminal {
                        TERMINAL_SUFFIX.width()
                    } else {
                        0
                    }
            })
            .max()
            .unwrap_or(0);
        // Marker, the key column at its widest, the name, a space and borders.
        let width = ((2 + FILTER_KEY_WIDTH + widest + 1 + 2) as u16).max(MIN_WIDTH);
        let tools = if self.tools.is_empty() { 0 } else { 2 };
        let height = 2 + 2 + self.head() + (self.items.len() as u16).max(1) + tools;
        float(area, width, height)
    }

    pub fn render(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        self.mouse_rows = Rect::default();
        self.mouse_tools = Rect::default();
        let area = self.panel(area);
        // The title says what follows an action, and the footer names what
        // Ctrl-X would make follow instead, as the other hints name where a
        // key leads.
        let title = match self.after {
            Then::Stay => " Actions ",
            Then::Quit => " Actions, then quit ",
            Then::Cd => " Actions, then cd ",
        };
        let footer = match (self.filtering, self.after == Then::Stay) {
            (true, true) => " Enter: run  Tab: keys  ^X: quit after  Esc ",
            (true, false) => " Enter: run  Tab: keys  ^X: stay after  Esc ",
            (false, true) => " Enter: run  Tab: filter  ^X: quit after  Esc ",
            (false, false) => " Enter: run  Tab: filter  ^X: stay after  Esc ",
        };
        let inner = open_panel(area, frame, title, footer);
        let head = self.head();
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
                Some(ch) => format!("{ch}{ARROW}"),
                None => " ".repeat(KEY_WIDTH),
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
            Paragraph::new(tail(
                &self.target.display().to_string(),
                target.width as usize,
            ))
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
            const WAITING: &str = "Press a key to run.";
            frame.render_widget(
                Paragraph::new(WAITING).style(Style::default().fg(Color::DarkGray)),
                prompt,
            );
            // The picker underneath put the cursor in its own prompt, where
            // typing no longer goes while the menu is open.
            if prompt.width > 0 && prompt.height > 0 {
                frame.set_cursor_position((
                    prompt.x + (WAITING.width() as u16 + 1).min(prompt.width - 1),
                    prompt.y,
                ));
            }
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
            (|ch| format!("alt+{ch}"), FILTER_KEY_WIDTH)
        } else {
            (|ch| ch.to_string(), KEY_WIDTH)
        };
        let items: Vec<ListItem> = shown
            .iter()
            .map(|&(row, index)| {
                let action = &self.items[index];
                let suffix = if action.run_mode() == RunMode::Terminal {
                    TERMINAL_SUFFIX
                } else {
                    ""
                };
                // The key, an arrow, the name: the key is what gets pressed, so
                // it is the bright part, and the arrow only joins the two.
                let (key, arrow) = match (keyed, self.shown_key(index)) {
                    (true, Some(ch)) => (label(ch), ARROW.to_string()),
                    (true, None) => (String::new(), " ".repeat(width)),
                    (false, _) => (String::new(), String::new()),
                };
                let line = Line::from(vec![
                    Span::raw(if row == self.selected { "▌ " } else { "  " }),
                    Span::styled(key, KEY_STYLE),
                    Span::styled(arrow, ARROW_STYLE),
                    Span::raw(action.name().to_string()),
                    Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                ]);
                let style = if row == self.selected {
                    SELECTED_STYLE
                } else {
                    Style::default()
                };
                ListItem::new(line).style(style)
            })
            .collect();
        frame.render_widget(List::new(items), rows);
    }
}

/// Where a panel of this size floats: the bottom right of `area`, inside the
/// picker's own border. A screen too small to float in gives it the whole area.
pub(crate) fn float(area: Rect, width: u16, height: u16) -> Rect {
    if area.width < width + 4 || area.height < height + 2 {
        return area;
    }
    Rect {
        x: area.right() - width - 2,
        y: area.bottom() - height - 1,
        width,
        height,
    }
}

/// Clears `area`, draws the frame every floating panel shares and returns the
/// space inside it. The picker is still drawn underneath, and the border has a
/// colour of its own so the panel is not taken for part of it.
pub(crate) fn open_panel(
    area: Rect,
    frame: &mut ratatui::Frame,
    title: &'static str,
    footer: &'static str,
) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Indexed(109)))
        .title(title)
        .title_bottom(Line::from(footer).centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// The end of `text` that fits in `width` columns, led by an ellipsis when
/// something was cut. The name at the end of a path is the part that says
/// which item the actions run on, so that is the end that is kept.
fn tail(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut used = 1;
    for ch in text.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        used += w;
        kept.insert(0, ch);
    }
    format!("…{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_x_switches_what_follows_an_action_between_staying_and_quitting() {
        let ctrl_x = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        let mut menu = Menu::new(PathBuf::from("x")).then(Then::Quit);
        assert!(matches!(menu.handle(ctrl_x), Decision::Stay));
        assert_eq!(menu.after, Then::Stay);
        menu.handle(ctrl_x);
        assert_eq!(menu.after, Then::Quit);
        // cd leaves the screen too, so from it the switch goes to stay.
        let mut menu = Menu::new(PathBuf::from("x")).then(Then::Cd);
        menu.handle(ctrl_x);
        assert_eq!(menu.after, Then::Stay);
    }
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn the_menu_floats_at_the_bottom_right_and_leaves_the_list_in_view() {
        let mut menu = menu_of(vec![Action::Reveal, Action::Copy]);
        let area = Rect::new(0, 0, 120, 30);
        let panel = menu.panel(area);
        // Inside the picker's own border, against its right and bottom edges.
        assert_eq!(panel.right(), area.right() - 2);
        assert_eq!(panel.bottom(), area.bottom() - 1);
        // Two actions, the target and prompt lines, the zone below and borders.
        assert_eq!(panel.height, 2 + 2 + 2 + 2);
        assert!(panel.width < area.width / 2, "{panel:?}");

        // What is already on screen to the left of it is not painted over.
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("x".repeat(120)), Rect::new(0, 25, 120, 1));
                menu.render(frame.area(), frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 25)].symbol(), "x");
        assert_eq!(buffer[(panel.x - 1, 25)].symbol(), "x");
        assert_ne!(buffer[(panel.x + 1, 25)].symbol(), "x");

        // The panel keeps its size while a filter hides rows, so it does not
        // jump about under the pointer.
        menu.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        menu.handle(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(menu.visible.is_empty());
        assert_eq!(menu.panel(area), panel);

        // Without room to float it takes the whole area, as a small terminal needs.
        let small = Rect::new(0, 0, 40, 9);
        assert_eq!(menu.panel(small), small);
    }

    #[test]
    fn a_long_target_keeps_its_name_in_sight() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail(r"C:\work\project\report.xlsx", 12), "…report.xlsx");
        // Wide characters are counted by the columns they take.
        assert_eq!(tail("資料/日本語.txt", 11), "…日本語.txt");
    }

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
            after: Then::Stay,
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
        assert_eq!(row(5), "  c → Copy path");
        assert_eq!(row(6), "─".repeat(44));
        assert_eq!(row(7), "      Open temporary copies folder");
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
        assert_eq!(row(&terminal, 6), "      Open temporary copies folder");

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
        assert_eq!(row(&terminal, 5), "      Open temporary copies folder");

        // It has no key; t opens a temporary copy of a file instead, and with
        // no file selected it does nothing. A click runs it.
        assert!(matches!(
            menu.handle(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT)),
            Decision::Stay
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
        assert_eq!(rows[0], "▌ f → Open in file manager");
        assert_eq!(rows[1], "      Copy path");
        assert_eq!(rows[2], "  c → Compare");

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
        assert_eq!(row(3), "▌ alt+f → Open in file manager");
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
            after: Then::Stay,
        };
        menu.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
        menu.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(menu.query.is_empty());
        assert_eq!(menu.visible, [0, 1]);
        menu.handle(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(menu.query, "d");
        // Ctrl-H deletes as Backspace does, as in the picker.
        menu.handle(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL));
        assert!(menu.query.is_empty());
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
            after: Then::Stay,
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
