use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config as MatchConfig, Matcher, Nucleo};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::browse::{self, Browser};
use crate::config::Config;
use crate::icons;
use crate::open;
use crate::scan::{self, Entry};
use crate::{Mode, PickArgs};

type Error = Box<dyn std::error::Error>;

/// Nucleo tick budget per frame. Keeps redraws under 16 ms while a scan is running.
const TICK_MS: u64 = 10;
const POLL: Duration = Duration::from_millis(16);
/// How long a confirmation stays before retiring itself.
const NOTICE_LINGER: Duration = Duration::from_secs(3);
/// How long clicks are swallowed after the screen is replaced wholesale.
///
/// Opening or closing the action menu puts different things under the pointer,
/// so a second click from a quick double tap would land on whatever moved into
/// that spot. Roughly a double-click interval is enough to catch those without
/// the screen feeling unresponsive.
const CLICK_GUARD: Duration = Duration::from_millis(300);
/// How long after Backspace deleted something a press on the empty filter
/// is still taken as clearing rather than as going up. Time alone decides:
/// letting go of the key does not end it, since a finger that lifts for a
/// moment and presses again is exactly the press too many this is for.
const ERASE_PAUSE: Duration = Duration::from_millis(1500);
/// The preview pane is dropped below this terminal width.
const MIN_WIDTH_FOR_PREVIEW: u16 = 80;
/// Directory entries read for the preview. Enough to fill any pane; bounds the cost on huge folders.
const PREVIEW_LIMIT: usize = 500;

const MODE_ORDER: [Mode; 5] = [
    Mode::Dirs,
    Mode::Files,
    Mode::Recent,
    Mode::Favorites,
    Mode::Browse,
];

/// One mode's candidates: its matcher and the scan that feeds it.
/// Sources are created the first time a mode is shown and kept, so switching
/// back with Tab is instant.
struct Source {
    mode: Mode,
    matcher: Nucleo<Entry>,
    scan_done: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    /// Set when the walk stopped at the configured ceiling, so the count on
    /// screen can say the list is short rather than look complete.
    truncated: Arc<AtomicBool>,
    scanner: Option<JoinHandle<Result<Vec<u32>, String>>>,
    scan_error: Option<String>,
    /// Which row is at the top of the list. See `scroll_window`.
    first: usize,
    /// The order the scan handed back, held until every item has reached the
    /// snapshot it indexes into.
    scanned_order: Option<Vec<u32>>,
    selected: u32,
    query_empty: bool,
    /// Item indices in browse order (shallow paths first, then by name), built
    /// once the scan has finished. With no query nucleo lists items in the order
    /// the parallel walk found them, which is noise to a reader.
    browse_order: Option<Vec<u32>>,
}

impl Source {
    /// `limit` is the ceiling for this scan alone, so a reader who asked for
    /// the rest of a folder gets it without editing a file and starting over.
    fn start(mode: Mode, root: &Path, config: &Config, limit: usize) -> Self {
        let matcher = Nucleo::new(MatchConfig::DEFAULT.match_paths(), Arc::new(|| {}), None, 1);
        let scan_done = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let truncated = Arc::new(AtomicBool::new(false));
        let scanner = scan::spawn(
            root.to_path_buf(),
            mode,
            &config.exclude,
            limit,
            matcher.injector(),
            scan::Signals {
                done: scan_done.clone(),
                cancel: cancel.clone(),
                truncated: truncated.clone(),
            },
        );
        Self {
            mode,
            matcher,
            scan_done,
            cancel,
            truncated,
            scanner: Some(scanner),
            scan_error: None,
            first: 0,
            selected: 0,
            query_empty: true,
            browse_order: None,
            scanned_order: None,
        }
    }

    /// Browse order applies to scanned modes with an empty query. zoxide's own
    /// order (by score) is the point of recent mode, so it is left alone.
    fn browsing(&self) -> bool {
        self.query_empty && !matches!(self.mode, Mode::Recent | Mode::Favorites)
    }

    /// Takes the order the scan built, once every item it names has reached
    /// the snapshot. The order itself is produced by the walk, so nothing is
    /// sorted here.
    fn refresh_browse_order(&mut self) {
        if matches!(self.mode, Mode::Recent | Mode::Favorites)
            || self.browse_order.is_some()
            || self.scanned_order.is_none()
            || !self.scan_done.load(Ordering::Acquire)
        {
            return;
        }
        let snapshot = self.matcher.snapshot();
        if snapshot.item_count() != self.matcher.injector().injected_items() {
            return;
        }
        self.browse_order = self.scanned_order.take();
    }

    /// The nth row as shown: browse order when browsing, nucleo's ranking otherwise.
    fn visible(&self, n: u32) -> Option<nucleo::Item<'_, Entry>> {
        let snapshot = self.matcher.snapshot();
        match (&self.browse_order, self.browsing()) {
            (Some(order), true) => snapshot.get_item(*order.get(n as usize)?),
            _ => snapshot.get_matched_item(n),
        }
    }

    fn set_query(&mut self, query: &str, append: bool) {
        self.matcher
            .pattern
            .reparse(0, query, CaseMatching::Ignore, Normalization::Smart, append);
        self.query_empty = query.is_empty();
        self.selected = 0;
    }

    fn selected_entry(&self) -> Option<Entry> {
        Some(self.visible(self.selected)?.data.clone())
    }

    fn clamp_selection(&mut self) {
        let count = self.matcher.snapshot().matched_item_count();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    /// Collect only completed workers, keeping slow history queries off the UI thread.
    /// Says whether the walk just finished, so the caller can retire the
    /// spinner and show the final count instead of leaving both mid-scan
    /// until the reader happens to press something.
    fn collect_scan_result(&mut self) -> bool {
        let finished = self
            .scanner
            .as_ref()
            .is_some_and(|handle| handle.is_finished());
        if finished {
            self.finish_scan();
        }
        finished
    }

    fn finish_scan(&mut self) {
        if let Some(handle) = self.scanner.take() {
            match handle
                .join()
                .unwrap_or_else(|_| Err("scan thread panicked".into()))
            {
                Ok(order) => self.scanned_order = Some(order),
                Err(error) => self.scan_error = Some(error),
            }
            self.scan_done.store(true, Ordering::Release);
        }
    }
}

impl Source {
    /// Throw the source away without making the caller wait for it. Dropping
    /// one waits for its scanner to notice the cancel flag and then frees a
    /// matcher holding every scanned path, which together cost tens of
    /// milliseconds on a large tree. Leaving a mode switch to pay that is
    /// what made switching stutter.
    fn retire(self) {
        self.cancel.store(true, Ordering::Relaxed);
        std::thread::spawn(move || drop(self));
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = self.scanner.take() {
            let _ = handle.join();
        }
    }
}

struct Picker {
    /// One per candidate source; the browse slot stays empty.
    sources: [Option<Source>; MODE_ORDER.len()],
    /// State of browse mode, created when the mode is first shown.
    browser: Option<Browser>,
    /// Whether browse is drawn as a tree rather than in columns. Kept while
    /// in a search, so Tab comes back to the layout left.
    tree_view: bool,
    /// The tree, rooted where browse is. Built when first drawn and again
    /// whenever browse has moved somewhere else.
    tree: Option<crate::tree::Tree>,
    /// What is typed in the tree, which searches everything under its root.
    tree_query: String,
    /// The scan behind that search: folders and files under the tree's root,
    /// started at the first letter typed.
    tree_search: Option<Source>,
    /// The folder that scan walked. The tree's root can move without the
    /// tree being built again, as Left on its top line does, and a scan of
    /// the old root must not answer for the new one.
    tree_search_root: PathBuf,
    /// The best matches of that search, under their folders. Shown instead
    /// of the tree while something is typed.
    found: Option<crate::tree::Tree>,
    /// Whether the row selected in `found` is kept when it is built again as
    /// more matches arrive: only once the reader has moved it. Until then the
    /// best match so far is selected, and a new query starts over.
    found_keep: bool,
    /// The matched characters of each row in `found`, as char positions in
    /// its name.
    found_marks: HashMap<PathBuf, Vec<u32>>,
    /// Rows the tree had on screen last time, so a search collects about as
    /// many matches as can be shown.
    tree_rows: usize,
    /// Whether each path shown by the search is a folder, so it is asked of
    /// the disk once rather than on every rebuild.
    tree_dirs: HashMap<PathBuf, bool>,
    /// The scan root browse was last lined up with.
    ///
    /// Browse keeps where it was, so flipping between it and a search stays
    /// where the reader left off. Once the search itself moves elsewhere the
    /// two are about different folders, and coming back to browse landed on
    /// whatever it had been looking at before, unrelated to the search.
    browser_root: PathBuf,
    /// The search Tab returns to from browse: the last mode used that is not
    /// browse. The one a picker opened in, so `cf` goes back to files.
    search_mode: Mode,
    /// Set once the reader asks for a list that stopped at the ceiling to be
    /// collected in full. Scans started afterwards ignore `scan_limit`.
    unlimited: bool,
    /// Where the search started before, and where it was on the way back.
    ///
    /// Leaving browse hands its location to the search modes, which is what
    /// makes browsing to a folder and then searching it one gesture. Wandering
    /// first and leaving second moved the search somewhere nobody chose, with
    /// no way back except to browse there again.
    roots_back: Vec<RootVisit>,
    roots_forward: Vec<RootVisit>,
    mode: Mode,
    query: String,
    root: PathBuf,
    config: Config,
    /// Recomputes match positions for the visible rows only.
    highlighter: Matcher,
    /// Directory listing shown on the right, keyed by the path it was read from.
    preview: Option<(PathBuf, Vec<String>)>,
    /// Drives the spinner.
    frame_count: u32,
    /// When a notice should disappear on its own. Confirmations say something
    /// that already finished, so they go quiet; failures wait to be read.
    notice_until: Option<std::time::Instant>,
    /// Until when a click is treated as left over from the previous screen.
    clicks_blocked_until: Option<std::time::Instant>,
    notice: Option<String>,
    pinned: crate::favorites::Index,
    menu: Option<crate::action_menu::Menu>,
    /// The folder whose favorite is being named, and the name typed so far,
    /// while the name prompt is open.
    naming: Option<(PathBuf, String)>,
    /// The panel of keys that Ctrl-Space opens.
    keys: Option<crate::key_menu::KeyMenu>,
    /// The list of places open over the screen, recent or favorites, and
    /// what is typed into it. Its source is the one kept for that mode.
    places: Option<Mode>,
    places_query: String,
    /// The rows of the list of places, for clicks and the wheel.
    mouse_places: (Rect, usize, usize),
    mouse_rows: (Rect, usize, usize),
    mouse_header: Rect,
    /// Clickable areas of the navigation buttons, empty outside browse mode.
    mouse_nav: Vec<(Rect, Nav)>,
    /// Clickable areas of the header path, one per step of the breadcrumb.
    mouse_crumbs: Vec<(Rect, PathBuf)>,
    /// Column where the mode tabs start, which the buttons push to the right.
    mouse_modes_x: u16,
    mouse_paths: Vec<(Rect, PathBuf)>,
    last_click: Option<(PathBuf, u16, u16, std::time::Instant)>,
    /// What follows an action run from the menu, for actions that do not
    /// say themselves. Set by --after-action and switched with Ctrl-X.
    after_action: crate::actions::Then,
    /// The folder handed to the shell when an action ended the run with cd.
    after_cd: Option<PathBuf>,
    /// When Backspace last deleted a letter, or was swallowed on the empty
    /// filter soon after. While that is recent, Backspace does not go up.
    last_erase: Option<std::time::Instant>,
}

/// A place the search has started from, with what had been typed there.
struct RootVisit {
    root: PathBuf,
    query: String,
}

/// How many steps each direction of the root history keeps, as in browse.
const ROOT_HISTORY: usize = 100;

/// The buttons drawn at the head of the header, mirroring the history keys
/// (Ctrl or Alt with the arrows) and Left, so the same moves are reachable
/// without the keyboard.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Nav {
    Back,
    Forward,
    Up,
}

impl Nav {
    /// Glyph and the width it is drawn in, padding included.
    const BUTTONS: [(Nav, &'static str); 3] =
        [(Nav::Back, " ◀ "), (Nav::Forward, " ▶ "), (Nav::Up, " ▲ ")];
    const WIDTH: u16 = 3;
}

enum Action {
    Continue,
    Accept,
    Cancel,
    Execute(Box<(crate::actions::Action, PathBuf)>),
}

pub fn run(args: PickArgs, root: PathBuf, config: Config) -> Result<Option<PathBuf>, Error> {
    let (pinned, notice) = match crate::favorites::Index::load() {
        Ok(index) => (index, None),
        Err(error) => (
            crate::favorites::Index::default(),
            Some(format!("Cannot load favorite markers: {error}")),
        ),
    };
    let (mode, refused) = opening_screen(args.mode, &root);
    let mut picker = Picker {
        sources: std::array::from_fn(|_| None),
        browser: (mode == Mode::Browse).then(|| Browser::new(root.clone())),
        browser_root: root.clone(),
        search_mode: first_search(args.mode),
        mode,
        query: String::new(),
        root,
        config,
        unlimited: false,
        roots_back: Vec::new(),
        roots_forward: Vec::new(),
        highlighter: Matcher::new(MatchConfig::DEFAULT.match_paths()),
        preview: None,
        frame_count: 0,
        notice_until: None,
        clicks_blocked_until: None,
        notice,
        pinned,
        menu: None,
        naming: None,
        keys: None,
        tree_view: false,
        tree: None,
        tree_query: String::new(),
        tree_search: None,
        tree_search_root: PathBuf::new(),
        found: None,
        found_keep: false,
        found_marks: HashMap::new(),
        tree_rows: 20,
        tree_dirs: HashMap::new(),
        places: None,
        places_query: String::new(),
        mouse_places: (Rect::default(), 0, 0),
        mouse_rows: (Rect::default(), 0, 0),
        mouse_header: Rect::default(),
        mouse_nav: Vec::new(),
        mouse_crumbs: Vec::new(),
        mouse_modes_x: 0,
        mouse_paths: Vec::new(),
        last_click: None,
        after_action: crate::actions::Then::Stay,
        after_cd: None,
        last_erase: None,
    };
    picker.after_action = args.after_action;
    if let Some(reason) = refused {
        picker.set_notice(reason);
    }
    picker.set_query(&args.query);

    if args.select_1 && args.mode != Mode::Browse {
        let source = picker.source();
        source.finish_scan();
        if let Some(error) = &source.scan_error {
            return Err(error.clone().into());
        }
        while source.matcher.tick(TICK_MS).running {}
        if source.matcher.snapshot().matched_item_count() == 0 {
            return Ok(None);
        }
        if source.matcher.snapshot().matched_item_count() == 1
            && let Some(entry) = source.selected_entry()
        {
            return Ok(Some(entry.output_path(args.mode)));
        }
    }

    // Opened into a list of places, as zi is, the list floats over browse of
    // the folder the shell is in, and Esc leaves that browse behind.
    if matches!(args.mode, Mode::Recent | Mode::Favorites) {
        picker.mode = Mode::Browse;
        picker.browser = Some(Browser::new(picker.root.clone()));
        picker.places_query = std::mem::take(&mut picker.query);
        picker.open_places(args.mode);
    }
    let result = picker.run_tui()?;
    // An action that ended the run with cd hands over its folder as it is.
    if let Some(folder) = picker.after_cd.take() {
        return Ok(Some(folder));
    }
    // A place chosen from a list is printed as it is, whatever screen the
    // list was over; only a file picked in files hands back its folder.
    let mode = if picker.places.is_some() {
        Mode::Browse
    } else {
        picker.mode
    };
    drop(picker);
    Ok(result.map(|e| e.output_path(mode)))
}

/// The screen a picker opens in, given the one asked for and where. A search
/// that cannot scan its root, such as `c :net` on a favorite that is a share,
/// opens in browse instead, and says why: a refused search shows nothing but
/// the refusal, while browse lists the folder one level at a time.
fn opening_screen(asked: Mode, root: &Path) -> (Mode, Option<String>) {
    if !matches!(asked, Mode::Dirs | Mode::Files) {
        return (asked, None);
    }
    match crate::scan::scan_allowed(root) {
        Ok(()) => (asked, None),
        Err(reason) => (Mode::Browse, Some(format!("Opened in browse: {reason}"))),
    }
}

fn mode_index(mode: Mode) -> usize {
    MODE_ORDER
        .iter()
        .position(|&m| m == mode)
        .expect("known mode")
}

/// The search a picker opened in `mode` goes back to from browse. Opened
/// into browse or a list of places it has not searched yet, so it gets the
/// directory search.
fn first_search(mode: Mode) -> Mode {
    if mode == Mode::Files {
        Mode::Files
    } else {
        Mode::Dirs
    }
}

/// The other search, for Shift-Tab: dirs and files take turns. Browse is
/// where Tab leads, and recent and favorites are lists over the screen.
fn next_search(mode: Mode) -> Mode {
    if mode == Mode::Dirs {
        Mode::Files
    } else {
        Mode::Dirs
    }
}

impl Picker {
    /// The current mode's source, started on first use.
    fn source(&mut self) -> &mut Source {
        self.source_for(self.mode)
    }

    /// The source of `mode`, started on first use. A list of places filters
    /// on what is typed into it, the searches on the prompt.
    fn source_for(&mut self, mode: Mode) -> &mut Source {
        let idx = mode_index(mode);
        if self.sources[idx].is_none() {
            let limit = if self.unlimited {
                0
            } else {
                self.config.scan_limit
            };
            let mut source = Source::start(mode, &self.root, &self.config, limit);
            let query = if matches!(mode, Mode::Recent | Mode::Favorites) {
                &self.places_query
            } else {
                &self.query
            };
            source.set_query(query, false);
            self.sources[idx] = Some(source);
        }
        self.sources[idx].as_mut().expect("just created")
    }

    /// Opens the list of recent places or favorites over the screen, or
    /// closes it when it is the one open. Favorites are read again each
    /// time, since another shell may have pinned something meanwhile.
    fn open_places(&mut self, mode: Mode) {
        if self.places == Some(mode) {
            self.places = None;
            return;
        }
        self.places_query.clear();
        self.restart_places(mode);
        self.places = Some(mode);
    }

    fn restart_places(&mut self, mode: Mode) {
        if mode == Mode::Favorites {
            self.reload_pinned();
        }
        if let Some(stale) = self.sources[mode_index(mode)].take() {
            stale.retire();
        }
        self.source_for(mode);
    }

    fn set_places_query(&mut self, query: &str) {
        let Some(mode) = self.places else {
            return;
        };
        let append = query.starts_with(&self.places_query);
        self.places_query = query.to_string();
        self.source_for(mode).set_query(query, append);
    }

    /// A place chosen from the list, to go on from: a search starts from it,
    /// and browse shows it in its folder, selected, as Tab shows a search
    /// result, rather than stepping inside where the highlight would be on
    /// a first row nobody chose.
    fn go_to_place(&mut self, path: &Path) {
        self.places = None;
        if self.mode == Mode::Browse {
            self.browser().reveal(path);
            if self.tree_view {
                self.tree().reveal(path);
            }
            self.preview = None;
            return;
        }
        let search = self.mode;
        self.browse_at(path);
        // A share cannot be searched, so the search is not resumed there;
        // browse of it is what there is.
        if let Err(reason) = crate::scan::scan_allowed(path) {
            self.set_notice(format!("Opened in browse: {reason}"));
            return;
        }
        self.switch_to(search);
        self.set_query("");
    }

    /// Keys while a list of places is open. It takes the keys a search
    /// takes, and Enter, Right and the actions work on the place selected.
    fn handle_places_key(&mut self, key: KeyEvent) -> Action {
        let Some(mode) = self.places else {
            return Action::Continue;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) => {
                if self.places_query.is_empty() {
                    self.places = None;
                } else {
                    self.set_places_query("");
                }
            }
            (KeyCode::Char('c'), true) => return Action::Cancel,
            // The key that opened the list closes it; the other one swaps.
            (KeyCode::Char('r'), true) => self.open_places(Mode::Recent),
            (KeyCode::Char('s'), true) => self.open_places(Mode::Favorites),
            (KeyCode::Char('d' | 'f'), true) | (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                self.places = None;
                return self.handle_key(key);
            }
            (KeyCode::Enter, _) => return Action::Accept,
            (KeyCode::Right, _) | (KeyCode::Char('l'), true) => match self.selected_path() {
                // Favorites keeps folders that have been deleted, so they
                // can be unpinned, and there is nowhere to go for one.
                Some(path) if !path.exists() => {
                    self.set_notice(format!("Not there any more: {}", path.display()));
                }
                Some(path) => self.go_to_place(&path),
                None => {}
            },
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => {
                let s = self.source_for(mode);
                s.selected = s.selected.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => {
                let s = self.source_for(mode);
                s.selected = s.selected.saturating_add(1);
            }
            (KeyCode::PageUp, _) => {
                let s = self.source_for(mode);
                s.selected = s.selected.saturating_sub(10);
            }
            (KeyCode::PageDown, _) => {
                let s = self.source_for(mode);
                s.selected = s.selected.saturating_add(10);
            }
            (KeyCode::Backspace, _) | (KeyCode::Char('h'), true) => {
                let mut query = self.places_query.clone();
                query.pop();
                self.set_places_query(&query);
            }
            (KeyCode::Char('u'), true) => self.set_places_query(""),
            (KeyCode::F(5), _) => self.restart_places(mode),
            (KeyCode::Char('b'), true) => self.toggle_pin(),
            (KeyCode::Char('n'), true) => self.start_naming(),
            (KeyCode::Char('p'), true) => {
                if let Some(target) = self.selected_path() {
                    self.menu = Some(crate::action_menu::Menu::new(target).then(self.after_action));
                }
            }
            (KeyCode::Char('o'), true) => {
                if let Some(path) = self.selected_path() {
                    self.report_open(&path, open::launch(&path));
                }
            }
            (KeyCode::Char('e'), true) => {
                if let Some(path) = self.selected_path() {
                    self.report_open(&path, open::reveal(&path));
                }
            }
            (KeyCode::Char(c), false) if crate::keys::is_typed_text(&key) => {
                let mut query = self.places_query.clone();
                query.push(c);
                self.set_places_query(&query);
            }
            _ => {}
        }
        Action::Continue
    }

    /// Pins or unpins the folder Ctrl-B is pressed on, and says which.
    fn toggle_pin(&mut self) {
        let target = self.pin_target();
        let message = match target {
            None => "Nothing selected. Switch to browse to pin this folder.".into(),
            Some(path) => match crate::favorites::path().and_then(|file| {
                let added = crate::favorites::update(&file, &path, None)?;
                crate::favorites::Index::read(&file).map(|index| (added, index))
            }) {
                Ok((added, index)) => {
                    self.pinned = index;
                    self.refresh_favorites_list();
                    format!(
                        "{}: {}",
                        if added { "Pinned" } else { "Unpinned" },
                        path.display()
                    )
                }
                Err(error) => format!("Cannot update favorites: {error}"),
            },
        };
        self.set_notice(message);
    }

    /// The list of favorites reads the file again next time it is shown,
    /// and straight away while it is open.
    fn refresh_favorites_list(&mut self) {
        self.sources[mode_index(Mode::Favorites)] = None;
        if self.places == Some(Mode::Favorites) {
            self.source_for(Mode::Favorites);
        }
    }

    /// Opens the name prompt on the folder Ctrl-N is pressed on, with the
    /// name it has, so a name can be changed as well as given.
    fn start_naming(&mut self) {
        match self.pin_target() {
            None => {
                self.set_notice("Nothing selected. Switch to browse to name this folder.".into())
            }
            Some(path) => {
                let current = crate::favorites::path()
                    .and_then(|file| crate::favorites::read(&file))
                    .ok()
                    .and_then(|all| {
                        all.into_iter()
                            .find(|f| f.path == path)
                            .and_then(|f| f.name)
                    })
                    .unwrap_or_default();
                self.naming = Some((path, current));
            }
        }
    }

    fn set_query(&mut self, query: &str) {
        if self.in_tree() {
            self.set_tree_query(query);
            return;
        }
        if self.mode == Mode::Browse {
            self.browser().set_filter(query);
            return;
        }
        let append = query.starts_with(&self.query);
        self.query = query.to_string();
        let q = self.query.clone();
        self.source().set_query(&q, append);
    }

    fn browser(&mut self) -> &mut Browser {
        let root = self.root.clone();
        self.browser.get_or_insert_with(|| Browser::new(root))
    }

    /// The tree for where browse is. Built again whenever browse has moved
    /// somewhere else, as its history and the columns can move it; the row
    /// selected in the columns starts out selected.
    fn tree(&mut self) -> &mut crate::tree::Tree {
        let root = self.browser().cwd.clone();
        if self.tree.as_ref().is_none_or(|tree| tree.root != root) {
            // A search covers the root it was typed under, so it ends there.
            self.end_tree_search();
            let select = self.browser().selected_path();
            self.tree = Some(crate::tree::Tree::new(root, select.as_deref()));
        }
        self.tree.as_mut().expect("just built")
    }

    /// The tree on screen: the search's matches while something is typed,
    /// the tree of folders otherwise.
    fn shown_tree(&mut self) -> &mut crate::tree::Tree {
        if self.found.is_none() {
            return self.tree();
        }
        self.found.as_mut().expect("a search is shown")
    }

    fn end_tree_search(&mut self) {
        self.tree_query.clear();
        self.found = None;
        self.found_marks.clear();
        self.tree_dirs.clear();
        if let Some(source) = self.tree_search.take() {
            source.retire();
        }
    }

    /// Types into the tree. The first letter starts a scan of everything
    /// under the root; the matches arrive as it goes.
    fn set_tree_query(&mut self, query: &str) {
        if query.is_empty() {
            self.tree_query.clear();
            self.found = None;
            self.found_marks.clear();
            // The scan is kept for the next query, which then starts from
            // nothing rather than from this one.
            if let Some(source) = self.tree_search.as_mut() {
                source.set_query("", false);
            }
            return;
        }
        let root = self.tree().root.clone();
        if root.as_os_str().is_empty() {
            self.set_notice("Open a drive to search it".into());
            return;
        }
        let append = query.starts_with(&self.tree_query);
        self.tree_query = query.to_string();
        if self.tree_search_root != root
            && let Some(stale) = self.tree_search.take()
        {
            stale.retire();
        }
        if self.tree_search.is_none() {
            self.tree_search_root = root.clone();
            let limit = if self.unlimited {
                0
            } else {
                self.config.scan_limit
            };
            self.tree_search = Some(Source::start(Mode::Browse, &root, &self.config, limit));
        }
        let source = self.tree_search.as_mut().expect("just started");
        source.set_query(query, append);
        source.matcher.tick(TICK_MS);
        self.found_keep = false;
        self.rebuild_found();
    }

    /// Builds the tree of the best matches again. The best match is selected,
    /// unless the reader has moved the selection since the query changed.
    fn rebuild_found(&mut self) {
        // Asked for first: it ends the search when the tree has moved to
        // another folder since, as going back in history moves it, and the
        // matches of the old folder must not be built under the new one.
        let root = self.tree().root.clone();
        let Some(source) = self.tree_search.as_ref() else {
            return;
        };
        let snapshot = source.matcher.snapshot();
        let take = snapshot
            .matched_item_count()
            .min(self.tree_rows.max(20) as u32);
        let pattern = snapshot.pattern().column_pattern(0);
        let mut marks = HashMap::new();
        let mut indices = Vec::new();
        let mut paths = Vec::new();
        for item in (0..take).filter_map(|n| snapshot.get_matched_item(n)) {
            indices.clear();
            pattern.indices(
                item.matcher_columns[0].slice(..),
                &mut self.highlighter,
                &mut indices,
            );
            indices.sort_unstable();
            indices.dedup();
            let path = item.data.path();
            spread_marks(&path, &item.data.display, &indices, &mut marks);
            paths.push(path);
        }
        self.found_marks = marks;
        let matches: Vec<(PathBuf, bool)> = paths
            .into_iter()
            .map(|path| {
                let is_dir = *self
                    .tree_dirs
                    .entry(path.clone())
                    .or_insert_with(|| path.is_dir());
                (path, is_dir)
            })
            .collect();
        let keep = self
            .found
            .as_ref()
            .filter(|_| self.found_keep)
            .and_then(|tree| tree.selected_path());
        let mut found = crate::tree::Tree::from_matches(root, &matches);
        if let Some(path) = keep
            && let Some(index) = found.nodes().iter().position(|node| node.path == path)
        {
            found.selected = index;
        }
        self.found = Some(found);
    }

    /// Moves the tree's search on while matches arrive. Says whether anything
    /// on screen may have changed.
    fn tick_tree_search(&mut self) -> bool {
        // Ends the search first when the tree has moved to another folder,
        // as going back in history moves it; the old folder's matches would
        // otherwise stay on screen until the next draw.
        self.tree();
        let Some(source) = self.tree_search.as_mut() else {
            return false;
        };
        let just_finished = source.collect_scan_result();
        let busy = !source.scan_done.load(Ordering::Acquire);
        let status = source.matcher.tick(if busy { TICK_MS } else { 0 });
        if (status.changed || just_finished) && !self.tree_query.is_empty() {
            self.rebuild_found();
        }
        just_finished || status.changed || status.running || busy
    }

    fn in_tree(&self) -> bool {
        self.mode == Mode::Browse && self.tree_view
    }

    /// Switches browse between its columns and the tree; from a search it
    /// opens browse as a tree. Leaving the tree, what was selected there is
    /// shown selected in the columns, in the folder holding it.
    fn toggle_tree(&mut self) {
        if self.mode != Mode::Browse {
            self.tree_view = true;
            self.switch_to(Mode::Browse);
            return;
        }
        if self.tree_view {
            let selected = self
                .found
                .as_ref()
                .or(self.tree.as_ref())
                .and_then(|tree| tree.selected_path());
            self.end_tree_search();
            if let Some(path) = selected
                && path != self.browser().cwd
            {
                self.browser().reveal(&path);
            }
            self.tree = None;
        }
        self.tree_view = !self.tree_view;
        self.preview = None;
    }

    /// A click on a tab. Browse's two tabs also pick how it is drawn, from a
    /// search as well as from browse.
    fn select_tab(&mut self, (mode, tree): Tab) {
        if mode != Mode::Browse {
            self.switch_to(mode);
        } else if self.mode == Mode::Browse {
            if self.tree_view != tree {
                self.toggle_tree();
            }
        } else if tree {
            self.toggle_tree();
        } else {
            self.end_tree_search();
            self.tree = None;
            self.tree_view = false;
            self.switch_to(Mode::Browse);
        }
    }

    /// Left in the tree: closes a folder, goes to the row above it, and from
    /// the root line moves the root up, as Left climbs in the columns.
    fn tree_left(&mut self) {
        if !self.tree().left() {
            return;
        }
        let Some(parent) = self.browser().parent_dir() else {
            return;
        };
        // The tree moves first, keeping its open branches, so that browse
        // arriving at the same place does not have it built again.
        self.tree().rise(parent);
        self.browser().up();
        self.preview = None;
    }

    fn handle_tree_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let searching = self.found.is_some();
        if searching
            && matches!(
                (key.code, ctrl),
                (
                    KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown,
                    _
                ) | (KeyCode::Char('j' | 'k' | 'h'), true)
                    | (KeyCode::Left, _)
            )
        {
            self.found_keep = true;
        }
        if key.code == KeyCode::Backspace {
            // Backspace deletes, and on an empty filter goes up as Left
            // does, unless it has just been clearing: see backspace_goes_up.
            if !self.tree_query.is_empty() {
                let mut query = self.tree_query.clone();
                query.pop();
                self.set_tree_query(&query);
                self.note_erase();
            } else if self.backspace_goes_up() {
                self.tree_left();
            }
            return Action::Continue;
        }
        match (key.code, ctrl) {
            // In a search Left walks up the matches' folders; the root stays.
            (KeyCode::Left, _) | (KeyCode::Char('h'), true) if searching => {
                self.shown_tree().left();
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), true) => self.tree_left(),
            // Right on a match goes back to the tree of folders, opened down
            // to it: a search finds the place, and the tree goes on from there.
            (KeyCode::Right, _) | (KeyCode::Char('l'), true) if searching => {
                self.leave_tree_search();
            }
            (KeyCode::Right, _) | (KeyCode::Char('l'), true) => self.tree().right(),
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => self.shown_tree().move_selection(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => self.shown_tree().move_selection(1),
            (KeyCode::PageUp, _) => self.shown_tree().move_selection(-10),
            (KeyCode::PageDown, _) => self.shown_tree().move_selection(10),
            (KeyCode::Char('u'), true) => self.set_tree_query(""),
            (KeyCode::Char(c), false) if crate::keys::is_typed_text(&key) => {
                let mut query = self.tree_query.clone();
                query.push(c);
                self.set_tree_query(&query);
            }
            _ => {}
        }
        Action::Continue
    }

    /// Leaves the tree's search for the tree of folders, opened down to the
    /// selected match: a search finds the place, and the tree goes on from
    /// there.
    fn leave_tree_search(&mut self) {
        if let Some(path) = self.shown_tree().selected_path() {
            self.set_tree_query("");
            self.tree().reveal(&path);
        }
    }

    /// Leaves a search for browse at `path`: inside a folder, or beside a file
    /// with the file selected. Tab from there searches from that folder, so a
    /// favorite or a search result is a way into the rest of the picker and
    /// not only a place to cd to.
    fn browse_at(&mut self, path: &Path) {
        self.mode = Mode::Browse;
        self.browser().navigate_to(path);
        self.preview = None;
    }

    /// Right on a search result: a look inside, in browse.
    fn go_into(&mut self, path: &Path) {
        self.browse_at(path);
    }

    /// Moves the scan root, remembering where the search came from.
    fn set_root(&mut self, root: PathBuf) {
        if root == self.root {
            return;
        }
        let leaving = RootVisit {
            root: std::mem::replace(&mut self.root, root),
            query: self.query.clone(),
        };
        if self.roots_back.len() == ROOT_HISTORY {
            self.roots_back.remove(0);
        }
        self.roots_back.push(leaving);
        // Going somewhere new ends the trail forward, as it does in browse.
        self.roots_forward.clear();
        self.restart_scans();
    }

    /// Steps back or forward through the places the search has started from,
    /// bringing back what had been typed at each one.
    fn root_history(&mut self, forward: bool) -> bool {
        let (from, to) = if forward {
            (&mut self.roots_forward, &mut self.roots_back)
        } else {
            (&mut self.roots_back, &mut self.roots_forward)
        };
        let Some(visit) = from.pop() else {
            return false;
        };
        to.push(RootVisit {
            root: std::mem::replace(&mut self.root, visit.root),
            query: std::mem::replace(&mut self.query, visit.query),
        });
        self.restart_scans();
        true
    }

    fn has_root_history(&self, forward: bool) -> bool {
        let stack = if forward {
            &self.roots_forward
        } else {
            &self.roots_back
        };
        !stack.is_empty()
    }

    /// Throws away every list, since each one only covers the place it was
    /// built for. They are released in the background: see `Source::retire`.
    fn restart_scans(&mut self) {
        let discarded = std::mem::replace(&mut self.sources, std::array::from_fn(|_| None));
        for source in discarded.into_iter().flatten() {
            source.retire();
        }
        self.preview = None;
    }

    /// Tab: from a search to browse, and from browse back to the search it
    /// came from.
    ///
    /// Looking for something and looking around are what the picker is
    /// switched between most, so they take one key each way.
    fn toggle_browse(&mut self) {
        let target = if self.mode == Mode::Browse {
            self.search_mode
        } else {
            Mode::Browse
        };
        self.switch_to(target);
    }

    /// Shift-Tab: the next tab in the same bracket. In a search that is the
    /// next kind of search; in browse it switches between the columns and
    /// the tree. Tab is the key that crosses between the two brackets.
    fn next_search_mode(&mut self) {
        if self.mode == Mode::Browse {
            self.toggle_tree();
        } else {
            self.switch_to(next_search(self.mode));
        }
    }

    /// First entry into browse uses the search selection; subsequent mode
    /// switches restore the existing browser, including its filter and selection.
    fn switch_to(&mut self, entering: Mode) {
        if matches!(entering, Mode::Recent | Mode::Favorites) {
            self.open_places(entering);
            return;
        }
        let leaving = self.mode;
        if entering == leaving {
            return;
        }
        if entering != Mode::Browse {
            self.search_mode = entering;
        }

        if leaving == Mode::Browse {
            // From the list of drives the search starts at the drive selected;
            // with none selected it stays where it was.
            let b = self.browser();
            let cwd = if b.at_drives() {
                b.target()
            } else {
                b.cwd.clone()
            };
            let before = self.root.clone();
            if !cwd.as_os_str().is_empty() {
                self.set_root(cwd);
            }
            // Browsing away and switching back moves what the search covers.
            // Saying so, and naming the way back, is what keeps a wander from
            // quietly becoming a search of somewhere else.
            if self.root != before {
                let root = self.root.display().to_string();
                self.set_notice(format!("Searching from {root}  Ctrl-Left: back"));
            }
            self.browser_root = self.root.clone();
        }
        self.mode = entering;
        // Lined up again whenever the search has moved since browse last saw
        // it, which is the only time keeping the old place is a surprise.
        if entering == Mode::Browse && (self.browser.is_none() || self.browser_root != self.root) {
            let selected = self.sources[mode_index(leaving)]
                .as_ref()
                .and_then(Source::selected_entry);
            let mut browser = Browser::new(self.root.clone());
            // Shown in place, not stepped into. The highlight in a list nobody
            // has moved is only its first row, and opening inside that hid the
            // folder the reader was looking at a moment earlier.
            if let Some(path) = browse_reveal(leaving, selected) {
                browser.reveal(&path);
            }
            self.browser = Some(browser);
            self.browser_root = self.root.clone();
        } else if entering != Mode::Browse {
            self.source();
        }
    }

    fn run_tui(&mut self) -> Result<Option<Entry>, Error> {
        let mut terminal = TerminalGuard::enter(self.config.mouse)?;
        // Redraw only when something actually changed. Drawing on every pass
        // kept a core busy for as long as the picker was open, with nothing
        // on screen moving.
        let mut dirty = true;
        loop {
            if self.mode != Mode::Browse {
                let source = self.source();
                let just_finished = source.collect_scan_result();
                // A finished list needs no time budget; spending one delayed
                // every keystroke behind it.
                let busy = !source.scan_done.load(Ordering::Acquire);
                let status = source.matcher.tick(if busy { TICK_MS } else { 0 });
                source.refresh_browse_order();
                source.clamp_selection();
                // While the list is still filling the count and the spinner
                // move on their own.
                dirty |= just_finished || status.changed || status.running || busy;
            }
            if let Some(mode) = self.places {
                let source = self.source_for(mode);
                let just_finished = source.collect_scan_result();
                let busy = !source.scan_done.load(Ordering::Acquire);
                let status = source.matcher.tick(if busy { TICK_MS } else { 0 });
                source.clamp_selection();
                dirty |= just_finished || status.changed || status.running || busy;
            }
            if self.in_tree() {
                dirty |= self.tick_tree_search();
            }
            dirty |= self.expire_notice();
            // Draw only once the input queue is empty. A burst of scroll
            // events then costs one redraw at the end instead of one per
            // notch, which is what made the picker stop answering during a
            // fast scroll through a large directory.
            if !event::poll(Duration::ZERO)? && dirty {
                // Only now, because working out what the preview should show
                // asks the filesystem whether the selection is a directory,
                // and nothing can have changed the answer since the last draw.
                self.refresh_preview();
                terminal.draw(|frame| self.render(frame.area(), frame))?;
                dirty = false;
            }

            if !event::poll(POLL)? {
                continue;
            }
            // Anything the reader did can change what belongs on screen.
            dirty = true;
            let key = match event::read()? {
                Event::Mouse(mouse) => {
                    // The menu reads its own clicks, so the guard against
                    // presses left over from the previous screen has to be
                    // applied before handing the event to it as well.
                    if matches!(mouse.kind, MouseEventKind::Down(_)) && self.clicks_blocked() {
                        continue;
                    }
                    if self.config.mouse
                        && let Some(menu) = &mut self.menu
                    {
                        if !menu.handle_mouse(mouse) {
                            continue;
                        }
                        // The menu is about to close under the pointer, so the
                        // rest of a quick double tap must not reach whatever
                        // takes its place.
                        self.block_clicks();
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
                    } else if self.config.mouse
                        && let Some(keys) = &mut self.keys
                    {
                        let Some(code) = keys.handle_mouse(mouse) else {
                            continue;
                        };
                        // Closing under the pointer, as the action menu does.
                        self.block_clicks();
                        KeyEvent::new(code, KeyModifiers::NONE)
                    } else {
                        self.handle_mouse(mouse);
                        continue;
                    }
                }
                Event::Key(key) => key,
                Event::Resize(..) => {
                    terminal.reopen()?;
                    continue;
                }
                _ => continue,
            };
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match self.handle_key(key) {
                Action::Continue => {}
                Action::Cancel => return Ok(None),
                Action::Execute(request) => {
                    let (action, target) = *request;
                    let name = action.name().to_string();
                    if action.run_mode() == crate::actions::RunMode::Terminal {
                        drop(terminal);
                        let action_screen = ActionScreen::enter()?;
                        eprintln!("\n{name}\nTarget: {}", target.display());
                        let result = action.execute_terminal(&target);
                        let message = match result {
                            Ok(status) => format!("{name}: {status}"),
                            Err(error) => format!("{name}: {error}"),
                        };
                        eprintln!("\n{message}\nPress Enter or Esc to return to tadoru.");
                        let pause = wait_for_return();
                        drop(action_screen);
                        terminal = TerminalGuard::enter(self.config.mouse)?;
                        self.set_notice(message);
                        pause?;
                        self.preview = None;
                        if let Some(browser) = &mut self.browser {
                            browser.refresh();
                        }
                    } else {
                        self.set_notice(match action.execute_detached(&target) {
                            Ok(Some(path)) => format!("Temporary copy: {}", path.display()),
                            Ok(None) => format!(
                                "{}: {}",
                                if matches!(action, crate::actions::Action::Copy) {
                                    "Copied"
                                } else {
                                    "Started"
                                },
                                name
                            ),
                            Err(error) => format!("{name}: {error}"),
                        });
                    }
                    // What follows: the action's own say, else the run's.
                    match action.then().unwrap_or(self.after_action) {
                        crate::actions::Then::Stay => {}
                        crate::actions::Then::Quit => return Ok(None),
                        crate::actions::Then::Cd => {
                            // A file's folder, since the shell cds there.
                            self.after_cd = Some(match target.parent() {
                                Some(dir) if !target.is_dir() => dir.to_path_buf(),
                                _ => target.clone(),
                            });
                            return Ok(None);
                        }
                    }
                }
                Action::Accept => {
                    if let Some(mode) = self.places {
                        if let Some(entry) = self.source_for(mode).selected_entry() {
                            return Ok(Some(entry));
                        }
                        continue;
                    }
                    if self.mode == Mode::Browse {
                        let target = if self.tree_view {
                            self.shown_tree().target()
                        } else {
                            self.browser().target()
                        };
                        // In the list of drives, with nothing matching the
                        // filter, there is nowhere to go.
                        if target.as_os_str().is_empty() {
                            continue;
                        }
                        return Ok(Some(Entry::absolute(target)));
                    }
                    if let Some(entry) = self.source().selected_entry() {
                        return Ok(Some(entry));
                    }
                }
            }
        }
    }

    /// The file or directory under the cursor, as is (files mode gives the
    /// file, not its parent). Browse mode falls back to the directory shown.
    fn selected_path(&mut self) -> Option<PathBuf> {
        if let Some(mode) = self.places {
            return self.source_for(mode).selected_entry().map(|e| e.path());
        }
        if self.in_tree() {
            return self.shown_tree().selected_path();
        }
        if self.mode == Mode::Browse {
            let b = self.browser();
            return b
                .selected_path()
                .or_else(|| (!b.at_drives()).then(|| b.cwd.clone()));
        }
        self.source().selected_entry().map(|e| e.path())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !self.config.mouse || self.menu.is_some() || self.keys.is_some() {
            return;
        }
        // A press arriving just after the screen was replaced is almost always
        // the tail of a double tap aimed at what used to be there. Wheel and
        // release events are harmless, so only presses are dropped.
        if matches!(mouse.kind, MouseEventKind::Down(_)) && self.clicks_blocked() {
            return;
        }
        let position = (mouse.column, mouse.row).into();
        // With a list of places open, a click on a row selects it, the wheel
        // moves through it, and a click anywhere else closes it.
        if let Some(mode) = self.places {
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(wanted) = self.place_button_at(mouse.column, mouse.row)
            {
                self.open_places(wanted);
                return;
            }
            let (rows, first, count) = self.mouse_places;
            let next = match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) if rows.contains(position) => {
                    let row = first + (mouse.row - rows.y) as usize;
                    if row >= count {
                        return;
                    }
                    self.source_for(mode).selected = row as u32;
                    // A double click goes to the place, as Right does and as
                    // a double click goes into a folder in browse. Enter,
                    // which would quit, stays on the keyboard as it does in
                    // every list.
                    if let Some(path) = self.selected_path() {
                        let now = std::time::Instant::now();
                        let double =
                            self.last_click
                                .take()
                                .is_some_and(|(previous, x, y, time)| {
                                    previous == path
                                        && x == mouse.column
                                        && y == mouse.row
                                        && now.duration_since(time) <= Duration::from_millis(500)
                                });
                        if double {
                            self.handle_places_key(KeyEvent::new(
                                KeyCode::Right,
                                KeyModifiers::NONE,
                            ));
                        } else {
                            self.last_click = Some((path, mouse.column, mouse.row, now));
                        }
                    }
                    return;
                }
                MouseEventKind::Down(_) => {
                    self.places = None;
                    return;
                }
                MouseEventKind::ScrollUp => {
                    self.source_for(mode).selected.saturating_sub(3) as usize
                }
                MouseEventKind::ScrollDown => (self.source_for(mode).selected.saturating_add(3)
                    as usize)
                    .min(count.saturating_sub(1)),
                _ => return,
            };
            self.source_for(mode).selected = next as u32;
            return;
        }
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        ) {
            self.last_click = None;
        }
        if matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left | MouseButton::Right)
        ) && let Some((_, path)) = self
            .mouse_paths
            .iter()
            .find(|(area, _)| area.contains(position))
        {
            self.last_click = None;
            let path = path.clone();
            if mouse.kind == MouseEventKind::Down(MouseButton::Right) {
                self.right_click(path, mouse.modifiers);
                return;
            }
            if self.mode != Mode::Browse {
                self.browse_at(&path);
                return;
            }
            if path == self.browser().cwd {
                self.browser().up();
            } else {
                self.browser().navigate_to(&path);
            }
            self.preview = None;
            return;
        }
        // The buttons and the path sit on the top border, which is outside the
        // header row, so they are tested against their own recorded areas.
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some((_, nav)) = self
                .mouse_nav
                .iter()
                .find(|(area, _)| area.contains(position))
            {
                self.navigate(*nav);
                return;
            }
            // A step of the path goes straight to that ancestor, which saves
            // pressing Left once per level. In the search modes the path is
            // the scan root, so the same click searches from there instead.
            if let Some((_, dir)) = self
                .mouse_crumbs
                .iter()
                .find(|(area, _)| area.contains(position))
            {
                let dir = dir.clone();
                self.clear_notice();
                if self.mode != Mode::Browse {
                    self.set_root(dir);
                    return;
                }
                if dir != self.browser().cwd {
                    self.browser().navigate_to(&dir);
                    self.preview = None;
                }
                return;
            }
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && self.mouse_header.contains(position)
        {
            let first = self.mouse_modes_x;
            for (tab, start, end) in tab_offsets() {
                if mouse.column >= first + start && mouse.column < first + end {
                    self.select_tab(tab);
                    return;
                }
            }
            if let Some(mode) = self.place_button_at(mouse.column, mouse.row) {
                self.open_places(mode);
                return;
            }
        }
        let (area, first, count) = self.mouse_rows;
        if !area.contains(position) || count == 0 {
            return;
        }
        let selected = if self.in_tree() {
            self.shown_tree().selected
        } else if self.mode == Mode::Browse {
            self.browser().selected
        } else {
            self.source().selected as usize
        };
        let next = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left | MouseButton::Right) => {
                let row = first + (mouse.row - area.y) as usize;
                if row >= count {
                    return;
                }
                row
            }
            MouseEventKind::ScrollUp => selected.saturating_sub(3),
            MouseEventKind::ScrollDown => selected.saturating_add(3).min(count - 1),
            _ => return,
        };
        if self.in_tree() {
            self.shown_tree().selected = next;
            self.found_keep = self.found.is_some();
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(path) = self.shown_tree().selected_path()
            {
                // A double click opens or closes the folder, as in a file
                // tree side panel.
                let now = std::time::Instant::now();
                let double = self
                    .last_click
                    .take()
                    .is_some_and(|(previous, x, y, time)| {
                        previous == path
                            && x == mouse.column
                            && y == mouse.row
                            && now.duration_since(time) <= Duration::from_millis(500)
                    });
                // In a search, opening a folder would read it into the list
                // of matches, where what it holds would pass for matches; so
                // there a double click leaves the search, as Right does.
                if double && self.found.is_some() {
                    self.leave_tree_search();
                } else if double {
                    self.tree().toggle();
                } else {
                    self.last_click = Some((path, mouse.column, mouse.row, now));
                }
            }
        } else if self.mode == Mode::Browse {
            self.browser().selected = next;
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(path) = self.browser().selected_path()
            {
                let now = std::time::Instant::now();
                let double = self
                    .last_click
                    .take()
                    .is_some_and(|(previous, x, y, time)| {
                        previous == path
                            && x == mouse.column
                            && y == mouse.row
                            && now.duration_since(time) <= Duration::from_millis(500)
                    });
                if double {
                    self.browser().enter();
                    self.preview = None;
                } else {
                    self.last_click = Some((path, mouse.column, mouse.row, now));
                }
            }
        } else {
            self.source().selected = next as u32;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Right)
            && let Some(target) = self.selected_path()
        {
            self.right_click(target, mouse.modifiers);
        }
    }

    fn right_click(&mut self, target: PathBuf, modifiers: KeyModifiers) {
        if modifiers.contains(KeyModifiers::CONTROL) {
            self.menu = Some(crate::action_menu::Menu::new(target).then(self.after_action));
            return;
        }
        let result = open::launch(&target);
        self.report_open(&target, result);
    }

    /// Says what happened to a request that hands the path to another program.
    ///
    /// Nothing on this screen changes when an application is launched, and it
    /// can take seconds to appear, so without a line here the click looks
    /// ignored and a failure is silent. The name is enough: the folder it came
    /// from is on screen already, and the full path crowds the line out.
    ///
    /// There is no progress to show. Handing the path over is where this
    /// program's part ends, so a spinner would be inventing work it cannot
    /// see the end of.
    fn report_open(&mut self, target: &Path, result: std::io::Result<()>) {
        let name = target
            .file_name()
            .unwrap_or(target.as_os_str())
            .to_string_lossy();
        match result {
            Ok(()) => self.set_transient_notice(format!("Opened: {name}")),
            Err(error) => self.set_notice(format!("Cannot open {name}: {error}")),
        }
    }

    /// Starts ignoring clicks, because what is under the pointer has just been
    /// replaced and the next one is most likely a leftover from the old screen.
    fn block_clicks(&mut self) {
        self.clicks_blocked_until = Some(std::time::Instant::now() + CLICK_GUARD);
        self.last_click = None;
    }

    /// Whether a button press should be discarded as belonging to the screen
    /// that was on show a moment ago.
    /// Backspace deleted a letter: for a while, a press on the empty filter
    /// clears rather than climbs.
    fn note_erase(&mut self) {
        self.last_erase = Some(std::time::Instant::now());
    }

    /// Whether Backspace on an empty filter goes up. It does, unless Backspace
    /// deleted something within ERASE_PAUSE: then it is a press too many
    /// while clearing, and moving would take the reader somewhere they never
    /// meant to go. A swallowed press keeps the pause going, so holding the
    /// key past the empty filter stays put for as long as it is held.
    fn backspace_goes_up(&mut self) -> bool {
        let now = std::time::Instant::now();
        if self
            .last_erase
            .is_some_and(|at| now.duration_since(at) <= ERASE_PAUSE)
        {
            self.last_erase = Some(now);
            return false;
        }
        true
    }

    /// The list whose button on the top border is at that cell, if any.
    fn place_button_at(&self, column: u16, row: u16) -> Option<Mode> {
        if !self.mouse_header.contains((column, row).into()) {
            return None;
        }
        let first = self.mouse_modes_x;
        place_buttons(self.mouse_header.width)
            .into_iter()
            .find(|&(_, start, end)| column >= first + start && column < first + end)
            .map(|(mode, ..)| mode)
    }

    fn clicks_blocked(&mut self) -> bool {
        match self.clicks_blocked_until {
            Some(until) if std::time::Instant::now() < until => true,
            Some(_) => {
                self.clicks_blocked_until = None;
                false
            }
            None => false,
        }
    }

    /// The folder Ctrl-B pins and Ctrl-N names: the selected folder, or the
    /// folder holding the selected file. None on the list of drives.
    fn pin_target(&mut self) -> Option<PathBuf> {
        if let Some(mode) = self.places {
            return self.source_for(mode).selected_entry().map(|e| e.path());
        }
        if self.in_tree() {
            Some(self.shown_tree().target()).filter(|path| !path.as_os_str().is_empty())
        } else if self.mode == Mode::Browse {
            Some(self.browser().target()).filter(|path| !path.as_os_str().is_empty())
        } else {
            let mode = self.mode;
            self.source()
                .selected_entry()
                .map(|entry| entry.output_path(mode))
        }
    }

    /// The name prompt: letters type, Backspace deletes, Enter saves and
    /// Esc leaves the favorite as it was. An empty name saved takes the
    /// name away; the folder stays pinned, and is pinned if it was not.
    fn handle_naming_key(&mut self, key: KeyEvent) {
        let Some((_, text)) = &mut self.naming else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.naming = None,
            KeyCode::Enter => {
                let (path, text) = self.naming.take().expect("open");
                let name = text.trim();
                let result = crate::favorites::path().and_then(|file| {
                    let outcome = if name.is_empty() {
                        crate::favorites::set_name(&file, &path, None)
                    } else {
                        crate::favorites::set_name(&file, &path, Some(name))
                    };
                    outcome.and_then(|()| crate::favorites::Index::read(&file))
                });
                match result {
                    Ok(index) => {
                        self.pinned = index;
                        self.sources[mode_index(Mode::Favorites)] = None;
                        self.set_transient_notice(if name.is_empty() {
                            format!("Name removed: {}", path.display())
                        } else {
                            format!("{} is now :{name}", path.display())
                        });
                    }
                    Err(error) => self.set_notice(format!("Cannot name the favorite: {error}")),
                }
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) if crate::keys::is_typed_text(&key) && !c.is_whitespace() => {
                text.push(c);
            }
            _ => {}
        }
    }

    /// A message that stays until the next action replaces it, for anything
    /// the reader has to act on.
    fn set_notice(&mut self, text: String) {
        self.notice = Some(text);
        self.notice_until = None;
    }

    /// A message that goes quiet on its own, for confirming something finished.
    fn set_transient_notice(&mut self, text: String) {
        self.notice = Some(text);
        self.notice_until = Some(std::time::Instant::now() + NOTICE_LINGER);
    }

    fn clear_notice(&mut self) {
        self.notice = None;
        self.notice_until = None;
    }

    /// Retires a confirmation once its time is up. Says whether it did, so the
    /// caller knows the screen needs redrawing.
    fn expire_notice(&mut self) -> bool {
        let due = self
            .notice_until
            .is_some_and(|until| std::time::Instant::now() >= until);
        if due {
            self.clear_notice();
        }
        due
    }

    /// Runs a navigation button. Kept beside the key handling it mirrors, so
    /// clicking and pressing the key cannot drift apart.
    fn navigate(&mut self, nav: Nav) {
        self.clear_notice();
        // In browse the buttons walk the folders visited; in the search modes
        // they walk the folders the search has started from. Both answer the
        // same question, which is how to get back to where this began.
        if self.mode != Mode::Browse {
            match nav {
                Nav::Up => match self.root.parent().map(Path::to_path_buf) {
                    Some(parent) => self.set_root(parent),
                    None => self.set_notice("Already at the top of the drive".into()),
                },
                Nav::Back | Nav::Forward => {
                    if self.root_history(nav == Nav::Forward) {
                        let root = self.root.display().to_string();
                        self.set_notice(format!("Searching from {root}"));
                    } else {
                        self.set_notice("No earlier search location".into());
                    }
                }
            }
            return;
        }
        match nav {
            Nav::Up => {
                if self.tree_view {
                    self.tree().selected = 0;
                    self.tree_left();
                } else {
                    self.browser().up();
                }
                self.preview = None;
            }
            Nav::Back | Nav::Forward => {
                if self.browser().history(nav == Nav::Forward) {
                    self.preview = None;
                } else {
                    self.set_notice("No available directory in history".into());
                }
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        self.last_click = None;
        // A filter cleared with Esc was meant to be cleared, so Backspace
        // right after it goes up without the pause.
        if key.code == KeyCode::Esc {
            self.last_erase = None;
        }
        if self.naming.is_some() {
            self.handle_naming_key(key);
            return Action::Continue;
        }
        if let Some(menu) = &mut self.menu {
            let decision = menu.handle(key);
            // Ctrl-X in the menu sets what follows for the rest of the run.
            self.after_action = menu.after;
            return match decision {
                crate::action_menu::Decision::Stay => Action::Continue,
                crate::action_menu::Decision::Close => {
                    self.menu = None;
                    Action::Continue
                }
                crate::action_menu::Decision::Run(action) => {
                    let target = menu.target.clone();
                    self.menu = None;
                    Action::Execute(Box::new((*action, target)))
                }
            };
        }
        if let Some(keys) = &mut self.keys {
            return match keys.handle(key) {
                crate::key_menu::Decision::Stay => Action::Continue,
                crate::key_menu::Decision::Close => {
                    self.keys = None;
                    Action::Continue
                }
                crate::key_menu::Decision::Run(command) => {
                    self.keys = None;
                    match command {
                        crate::key_menu::Command::Go(mode) => {
                            self.switch_to(mode);
                            Action::Continue
                        }
                        crate::key_menu::Command::ToggleTree => {
                            self.toggle_tree();
                            Action::Continue
                        }
                        // Pressed for the reader, so a row and its shortcut
                        // can never come to do different things.
                        crate::key_menu::Command::Press(code, modifiers) => {
                            self.handle_key(KeyEvent::new(code, modifiers))
                        }
                    }
                }
            };
        }
        self.clear_notice();
        if self.places.is_some() {
            return self.handle_places_key(key);
        }
        // Some terminals send Ctrl-Space as a bare NUL.
        if key.code == KeyCode::Null
            || (key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::CONTROL)
        {
            self.keys = Some(crate::key_menu::KeyMenu::new(self.mode, self.in_tree()));
            return Action::Continue;
        }
        // Ctrl does the same as Alt here. Terminals that use Alt with the
        // arrows to move between panes never pass those keys on, and a history
        // that only answers to them cannot be reached there at all.
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            self.navigate(if key.code == KeyCode::Right {
                Nav::Forward
            } else {
                Nav::Back
            });
            return Action::Continue;
        }
        // Ctrl-T goes back too, as it does after a tag jump in Vim. AltGr
        // arrives as Ctrl+Alt on Windows and types a character, so Alt rules
        // it out.
        if key.code == KeyCode::Char('t') && key.modifiers == KeyModifiers::CONTROL {
            self.navigate(Nav::Back);
            return Action::Continue;
        }
        // Straight to a search by its letter; favorites are the starred ones.
        // Shift-Tab only steps forward, so by that key alone favorites is
        // three presses from dirs and four from browse. Ctrl with a letter
        // rather than Alt with a digit, which a terminal may keep for
        // switching its own tabs.
        // AltGr arrives as Ctrl+Alt on Windows and types a character.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
            && let KeyCode::Char(letter) = key.code
            && let Some(mode) = match letter.to_ascii_lowercase() {
                'd' => Some(Mode::Dirs),
                'f' => Some(Mode::Files),
                'r' => Some(Mode::Recent),
                's' => Some(Mode::Favorites),
                _ => None,
            }
        {
            self.switch_to(mode);
            return Action::Continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('p'), true) => {
                let target = self.selected_path().unwrap_or_else(|| self.root.clone());
                self.menu = Some(crate::action_menu::Menu::new(target).then(self.after_action));
                return Action::Continue;
            }
            (KeyCode::Char('n'), true) => {
                self.start_naming();
                return Action::Continue;
            }
            (KeyCode::Char('b'), true) => {
                self.toggle_pin();
                return Action::Continue;
            }
            (KeyCode::F(5), _) => {
                self.reload_pinned();
                self.preview = None;
                if self.in_tree() {
                    self.tree().refresh();
                } else if self.mode == Mode::Browse {
                    self.browser().refresh();
                } else {
                    self.sources[mode_index(self.mode)] = None;
                    self.source();
                }
                return Action::Continue;
            }
            (KeyCode::Esc, _) => {
                let has_filter = if self.in_tree() {
                    !self.tree_query.is_empty()
                } else if self.mode == Mode::Browse {
                    !self.browser().filter.is_empty()
                } else {
                    !self.query.is_empty()
                };
                if has_filter {
                    self.set_query("");
                    return Action::Continue;
                }
                return Action::Cancel;
            }
            (KeyCode::Char('c'), true) => return Action::Cancel,
            (KeyCode::Enter, _) => return Action::Accept,
            (KeyCode::Tab, _) => {
                self.toggle_browse();
                return Action::Continue;
            }
            (KeyCode::BackTab, _) => {
                self.next_search_mode();
                return Action::Continue;
            }
            // Hand the selection to the desktop and stay open. O opens, as o
            // does in the action menu and in yazi; E shows it in Explorer.
            (KeyCode::Char('o'), true) => {
                if let Some(path) = self.selected_path() {
                    self.report_open(&path, open::launch(&path));
                }
                return Action::Continue;
            }
            (KeyCode::Char('e'), true) => {
                if let Some(path) = self.selected_path() {
                    self.report_open(&path, open::reveal(&path));
                }
                return Action::Continue;
            }
            _ => {}
        }
        if self.mode == Mode::Browse {
            return self.handle_browse_key(key);
        }
        match (key.code, ctrl) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => {
                let s = self.source();
                s.selected = s.selected.saturating_sub(1);
                Action::Continue
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => {
                let s = self.source();
                s.selected = s.selected.saturating_add(1);
                Action::Continue
            }
            // Backspace deletes, and on an empty filter widens the search
            // as Left does, unless it has just been clearing: see
            // backspace_goes_up.
            (KeyCode::Backspace, _) => {
                if !self.query.is_empty() {
                    let mut q = self.query.clone();
                    q.pop();
                    self.set_query(&q);
                    self.note_erase();
                } else if self.backspace_goes_up() {
                    self.navigate(Nav::Up);
                }
                Action::Continue
            }
            (KeyCode::Char('u'), true) => {
                self.set_query("");
                Action::Continue
            }
            // Offered only while a list is short, and announced there rather
            // than in the key hints, which stay the same all session.
            (KeyCode::Char('a'), true) => {
                if self.source().truncated.load(Ordering::Acquire) {
                    self.unlimited = true;
                    let idx = mode_index(self.mode);
                    if let Some(stale) = self.sources[idx].take() {
                        stale.retire();
                    }
                    self.source();
                    self.set_notice("Collecting the rest of this folder".into());
                }
                Action::Continue
            }
            // The same key that climbs a level in browse. Here the level is
            // the root the scan started from, so the search widens by one.
            // Ctrl-H is Left, as Ctrl-J, Ctrl-K and Ctrl-L are the other
            // arrows, on every screen.
            (KeyCode::Left, _) | (KeyCode::Char('h'), true) => {
                self.navigate(Nav::Up);
                Action::Continue
            }
            // The same key that goes down a level in browse, and the same move
            // as a click on the preview. Enter would cd there and quit.
            (KeyCode::Right, _) | (KeyCode::Char('l'), true) => {
                match self.selected_path() {
                    // Favorites keeps folders that have been deleted, so they
                    // can be unpinned, and browse has nowhere to open for one.
                    Some(path) if !path.exists() => {
                        self.set_notice(format!("Not there any more: {}", path.display()));
                    }
                    Some(path) => self.go_into(&path),
                    None => {}
                }
                Action::Continue
            }
            (KeyCode::Char(c), false) if crate::keys::is_typed_text(&key) => {
                let mut q = self.query.clone();
                q.push(c);
                self.set_query(&q);
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> Action {
        if self.tree_view {
            return self.handle_tree_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Backspace deletes, and on an empty filter goes up as in yazi,
        // unless it has just been clearing: see backspace_goes_up.
        if key.code == KeyCode::Backspace {
            let b = self.browser();
            if !b.filter.is_empty() {
                let mut f = b.filter.clone();
                f.pop();
                b.set_filter(&f);
                self.note_erase();
            } else if self.backspace_goes_up() {
                self.browser().up();
            }
            return Action::Continue;
        }
        let b = self.browser();
        match (key.code, ctrl) {
            (KeyCode::Left, _) | (KeyCode::Char('h'), true) => b.up(),
            (KeyCode::Right, _) | (KeyCode::Char('l'), true) => b.enter(),
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => b.move_selection(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => b.move_selection(1),
            (KeyCode::PageUp, _) => b.move_selection(-10),
            (KeyCode::PageDown, _) => b.move_selection(10),
            (KeyCode::Char('u'), true) => b.set_filter(""),
            (KeyCode::Char(c), false) if crate::keys::is_typed_text(&key) => {
                let mut f = b.filter.clone();
                f.push(c);
                b.set_filter(&f);
            }
            _ => {}
        }
        Action::Continue
    }

    fn reload_pinned(&mut self) {
        match crate::favorites::Index::load() {
            Ok(index) => self.pinned = index,
            Err(error) => self.set_notice(format!("Cannot load favorite markers: {error}")),
        }
    }

    /// The screen, with the action menu floating over it while one is open.
    /// The list stays in view under the menu, so the item the actions are
    /// about to run on can still be seen.
    fn render(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        self.render_screen(area, frame);
        if let Some(mode) = self.places {
            self.render_places(mode, area, frame);
        }
        if let Some(menu) = &mut self.menu {
            menu.render(area, frame);
        }
        if let Some(keys) = &mut self.keys {
            keys.render(area, frame);
        }
        if let Some((path, text)) = &self.naming {
            // One line over the bottom border, where the key hints were, so
            // the list and the folder being named stay in view.
            let row = Rect::new(
                area.x,
                area.y + area.height.saturating_sub(1),
                area.width,
                1,
            );
            // The folder's own name: the end of a path is what tells folders
            // apart, and the row has no room for all of it.
            let shown = path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            let lead = format!(" Name {} as :", fit(&shown, area.width as usize / 3));
            let hint = "  Enter: save  Esc: cancel ";
            let line = Line::from(vec![
                Span::styled(lead.clone(), theme::HEADER),
                Span::styled(text.clone(), theme::PROMPT),
                Span::styled(hint, theme::INFO),
            ]);
            frame.render_widget(ratatui::widgets::Clear, row);
            frame.render_widget(Paragraph::new(line), row);
            let x = (lead.width() + text.width()) as u16;
            if x < area.width {
                frame.set_cursor_position((row.x + x, row.y));
            }
        }
    }

    fn render_screen(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        self.mouse_rows = (Rect::default(), 0, 0);
        self.mouse_header = Rect::default();
        self.mouse_nav.clear();
        self.mouse_crumbs.clear();
        self.mouse_paths.clear();
        if self.in_tree() {
            self.render_tree(area, frame);
            return;
        }
        if self.mode == Mode::Browse {
            self.render_browse(area, frame);
            return;
        }
        let mode = self.mode;
        let show_preview = area.width >= MIN_WIDTH_FOR_PREVIEW;
        let [list_area, preview_area] = if show_preview {
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area)
        } else {
            [area, Rect::default()]
        };

        let selected = self.source().selected_entry();
        if show_preview {
            let dir = selected
                .as_ref()
                .map(|e| e.output_path(mode))
                .filter(|p| p.is_dir());
            self.render_preview(preview_area, frame, dir.as_deref());
        }

        // Copied out before the source is borrowed, so the header can be laid
        // out without holding a second borrow of the picker.
        let root = self.root.clone();
        let history = [self.has_root_history(false), self.has_root_history(true)];
        let source = self.sources[mode_index(mode)]
            .as_mut()
            .expect("current source");
        let snapshot = source.matcher.snapshot();
        let count = snapshot.matched_item_count();
        let total = snapshot.item_count();
        let scanning = !source.scan_done.load(Ordering::Acquire);
        let truncated = source.truncated.load(Ordering::Acquire);

        let hints: Vec<String> = vec![
            "Tab: browse".to_string(),
            "^Space: keys".to_string(),
            format!("S-Tab: {}", next_search(mode).label()),
            "Enter: cd".to_string(),
            "Right: browse it".to_string(),
            "^S: favorites".to_string(),
            "Esc: clear/exit".to_string(),
            "^P: actions".to_string(),
            "^B: pin".to_string(),
            "F5: refresh".to_string(),
        ];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::BORDER)
            .title(Line::from(mode_tabs(
                (mode, false),
                self.places,
                list_area.width,
            )))
            // Shift-Tab is named by where it leads, as Tab is in browse.
            .title_bottom(footer_line(
                self.notice.as_deref(),
                &fit_hints(
                    &hints.iter().map(String::as_str).collect::<Vec<_>>(),
                    list_area.width.saturating_sub(2) as usize,
                ),
            ));
        let inner = block.inner(list_area);
        frame.render_widget(block, list_area);

        // Same vertical order as fzf --layout=reverse: prompt, info, header, list.
        let [prompt_area, info_area, header_area, rows_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

        // The tabs live on the top border now, so that row answers their clicks.
        self.mouse_header = Rect::new(list_area.x, list_area.y, list_area.width, 1);
        self.mouse_modes_x = list_area.x + 2;
        let prompt = Paragraph::new(Line::from(vec![
            Span::styled("> ", theme::PROMPT),
            Span::raw(&self.query),
        ]));
        frame.render_widget(prompt, prompt_area);
        frame.set_cursor_position((
            prompt_area.x + 2 + self.query.chars().count() as u16,
            prompt_area.y,
        ));

        let spinner = if scanning {
            self.frame_count = self.frame_count.wrapping_add(1);
            SPINNER[(self.frame_count / 4) as usize % SPINNER.len()]
        } else {
            ' '
        };
        let counts = format!("{spinner} {count}/{total} ");
        // A list that stopped early looks complete otherwise, and the reader
        // would take a missing folder for one that is not there.
        let capped = if truncated {
            "stopped at scan_limit, ^A: collect the rest "
        } else {
            ""
        };
        let rule_width = (info_area.width as usize)
            .saturating_sub(counts.chars().count())
            .saturating_sub(capped.chars().count());
        let info = Paragraph::new(Line::from(vec![
            Span::styled(counts, theme::INFO),
            Span::styled(capped, theme::NOTICE_BAD),
            Span::styled("─".repeat(rule_width), theme::BORDER),
        ]));
        frame.render_widget(info, info_area);
        // The tabs sit in the top border and the path gets a row of its own,
        // so a long path is not read as more tabs.
        // The buttons walk the places the search has started from, so they
        // belong on every mode that has a scan root, not only on browse.
        let (mut header, nav, after) = nav_buttons(
            header_area,
            [history[0], history[1], root.parent().is_some()],
        );
        self.mouse_nav = nav;
        let rest = Rect {
            x: after,
            width: header_area.width.saturating_sub(after - header_area.x),
            ..header_area
        };
        // The path a scan started from is also the way out of it: clicking
        // a step above the current one searches from there instead.
        let (spans, crumbs) = search_location(&root, rest);
        header.extend(spans);
        self.mouse_crumbs = crumbs;
        frame.render_widget(Paragraph::new(Line::from(header)), header_area);

        if let Some(error) = &source.scan_error {
            frame.render_widget(
                Paragraph::new(format!(
                    "Cannot load {}: {error}\nTab: browse  F5: retry  Esc: cancel",
                    mode.label()
                ))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false }),
                rows_area,
            );
            return;
        }
        let height = rows_area.height as u32;
        if height == 0 || count == 0 {
            return;
        }
        let selected = source.selected;
        source.first = scroll_window(
            source.first,
            selected as usize,
            count as usize,
            height as usize,
        );
        let first = source.first as u32;
        let last = (first + height).min(count);
        self.mouse_rows = (rows_area, first as usize, count as usize);
        let pattern = snapshot.pattern().column_pattern(0);
        let mut indices = Vec::new();
        let items: Vec<ListItem> = (first..last)
            .filter_map(|idx| source.visible(idx).map(|item| (idx, item)))
            .map(|(idx, item)| {
                indices.clear();
                pattern.indices(
                    item.matcher_columns[0].slice(..),
                    &mut self.highlighter,
                    &mut indices,
                );
                indices.sort_unstable();
                indices.dedup();
                let current = idx == selected;
                let mut line = highlight_line(current, &item.data.display, &indices);
                let marks = RowMarks {
                    icons: self.config.icons,
                    favorite: self.pinned.contains(&item.data.output_path(mode)),
                    inside: false,
                };
                let icon = marks.prefix(&item.data.display, mode != Mode::Files);
                if !icon.is_empty() {
                    line.spans.insert(1, icons::span(icon));
                }
                if current {
                    line = line.style(theme::CURRENT);
                }
                ListItem::new(line)
            })
            .collect();
        frame.render_widget(List::new(items), rows_area);
        // The right border is the track, so a list that ends at the bottom
        // edge of the screen cannot pass for the whole result.
        draw_scroll_thumb(
            frame.buffer_mut(),
            list_area.right().saturating_sub(1),
            rows_area.y,
            rows_area.height,
            first as usize,
            count as usize,
        );
    }

    /// The navigation buttons and the path, on a row of their own below the
    /// mode tabs.
    ///
    /// A path is long and changes with every move, so it gets a row to itself
    /// rather than trailing a list of tabs that never change. Records where
    /// each piece lands so the buttons and every step of the path can be
    /// clicked.
    fn browse_location(&mut self, area: Rect) -> Vec<Span<'static>> {
        let cwd = self.browser().cwd.clone();
        let available = [
            self.browser().has_history(false),
            self.browser().has_history(true),
            self.browser().parent_dir().is_some(),
        ];
        self.mouse_nav.clear();
        self.mouse_crumbs.clear();

        let mut x = area.x;
        let limit = area.right();
        let mut title: Vec<Span<'static>> = Vec::new();
        for ((nav, glyph), enabled) in Nav::BUTTONS.iter().zip(available) {
            title.push(Span::styled(
                *glyph,
                if enabled {
                    theme::HEADER
                } else {
                    theme::BORDER
                },
            ));
            if enabled && x + Nav::WIDTH <= limit {
                self.mouse_nav
                    .push((Rect::new(x, area.y, Nav::WIDTH, 1), *nav));
            }
            x += Nav::WIDTH;
        }
        title.push(Span::raw(" "));
        x += 1;
        if self.pinned.contains(&cwd) {
            let star = icons::span("★ ");
            x += star.content.width() as u16;
            title.push(star);
        }
        if cwd.as_os_str().is_empty() {
            title.push(Span::styled("Drives", theme::HERE_PATH));
        }
        for (span, dir) in crumb_spans(&cwd) {
            let width = span.content.width() as u16;
            // A path too long for the row is clipped, and the part that is not
            // drawn must not answer clicks.
            if let Some(dir) = dir
                && x + width <= limit
            {
                self.mouse_crumbs
                    .push((Rect::new(x, area.y, width, 1), dir));
            }
            x += width;
            title.push(span);
        }
        title.push(Span::raw(" "));
        title
    }

    /// Miller columns: parent, current directory, selected entry's contents. The
    /// arrangement predates every terminal file manager; Finder's column view and
    /// ranger use it too.
    /// Browse as a tree: the tree on the left, where the columns were, and
    /// the selected folder's contents on the right, as in a search.
    fn render_tree(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        let show_preview = area.width >= MIN_WIDTH_FOR_PREVIEW;
        let [list_area, preview_area] = if show_preview {
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area)
        } else {
            [area, Rect::default()]
        };
        if show_preview {
            let dir = self.shown_tree().selected_path().filter(|p| p.is_dir());
            self.render_preview(preview_area, frame, dir.as_deref());
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::BORDER)
            .title(Line::from(mode_tabs(
                (Mode::Browse, true),
                self.places,
                list_area.width,
            )))
            .title_bottom(footer_line(
                self.notice.as_deref(),
                &fit_hints(
                    &[
                        &format!("Tab: {}", self.search_mode.label()),
                        "S-Tab: browse",
                        "^Space: keys",
                        "Enter: cd",
                        "Right: open",
                        "Left: close",
                        "Ctrl/Alt-Left/Right: history",
                        "^P: actions",
                        "^B: pin",
                    ],
                    list_area.width.saturating_sub(2) as usize,
                ),
            ));
        let inner = block.inner(list_area);
        frame.render_widget(block, list_area);
        let [prompt_area, info_area, header_area, rows_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);
        self.mouse_header = Rect::new(list_area.x, list_area.y, list_area.width, 1);
        self.mouse_modes_x = list_area.x + 2;
        let location = self.browse_location(header_area);
        frame.render_widget(Paragraph::new(Line::from(location)), header_area);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", theme::PROMPT),
                Span::raw(self.tree_query.clone()),
            ])),
            prompt_area,
        );
        if prompt_area.width > 2 {
            let x = (2 + self.tree_query.width()).min(prompt_area.width as usize - 1);
            frame.set_cursor_position((prompt_area.x + x as u16, prompt_area.y));
        }
        // While searching: how many matched, and how many of them are shown.
        let label = match (&self.tree_search, &self.found) {
            // The scan was refused, as on a network drive: the folders can
            // still be opened one at a time.
            (Some(source), _) if source.scan_error.is_some() => format!(
                " Not searched: {} Right opens folders one at a time. ",
                source.scan_error.as_deref().unwrap_or_default()
            ),
            (Some(source), Some(found)) => {
                let snapshot = source.matcher.snapshot();
                let count = snapshot.matched_item_count();
                let shown = found.nodes().iter().filter(|node| !node.dim).count() - 1;
                let spinner = if source.scan_done.load(Ordering::Acquire) {
                    ' '
                } else {
                    self.frame_count = self.frame_count.wrapping_add(1);
                    SPINNER[(self.frame_count / 4) as usize % SPINNER.len()]
                };
                let truncated = if source.truncated.load(Ordering::Acquire) {
                    ", stopped at scan_limit"
                } else {
                    ""
                };
                if (count as usize) > shown {
                    format!(
                        "{spinner} {count}/{} matched, best {shown} shown{truncated} ",
                        snapshot.item_count()
                    )
                } else {
                    format!(
                        "{spinner} {count}/{} matched{truncated} ",
                        snapshot.item_count()
                    )
                }
            }
            _ => " tree ".to_string(),
        };
        let label = label.as_str();
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label, theme::INFO),
                Span::styled(
                    "─".repeat((info_area.width as usize).saturating_sub(label.width())),
                    theme::BORDER,
                ),
            ])),
            info_area,
        );

        self.tree();
        self.tree_rows = rows_area.height as usize;
        let icons = self.config.icons;
        let found_marks = &self.found_marks;
        let tree = match self.found.as_mut() {
            Some(found) => found,
            None => self.tree.as_mut().expect("built above"),
        };
        let height = rows_area.height as usize;
        let count = tree.nodes().len();
        tree.first = scroll_window(tree.first, tree.selected, count, height);
        let first = tree.first;
        let width = rows_area.width as usize;
        let rows: Vec<ListItem> = tree
            .nodes()
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
            .map(|(i, node)| {
                let current = i == tree.selected;
                let marks = RowMarks {
                    icons,
                    favorite: node.is_dir && self.pinned.contains(&node.path),
                    inside: node.open,
                };
                let icon = marks.prefix(&node.label, node.is_dir);
                let room = width.saturating_sub(2 + node.guide.width() + icon.width());
                // Folders shown only because a match sits in them are dimmed,
                // so the matches themselves stand out.
                let style = match (current, node.depth == 0, node.dim, node.is_dir) {
                    (true, _, _, _) => theme::CURRENT,
                    (false, true, _, _) => theme::HERE_PATH,
                    (false, false, true, _) => theme::SIDE_DIR,
                    (false, false, false, true) => theme::DIR,
                    (false, false, false, false) => Style::default(),
                };
                let pointer = if current {
                    Span::styled("▌ ", theme::POINTER)
                } else {
                    Span::raw("  ")
                };
                ListItem::new(
                    Line::from(
                        vec![
                            pointer,
                            Span::styled(node.guide.clone(), theme::BORDER),
                            icons::span(icon),
                        ]
                        .into_iter()
                        .chain(match_spans(
                            &fit(&node.label, room),
                            found_marks.get(&node.path).map_or(&[], Vec::as_slice),
                        ))
                        .collect::<Vec<_>>(),
                    )
                    .style(style),
                )
            })
            .collect();
        frame.render_widget(List::new(rows), rows_area);
        self.mouse_rows = (rows_area, first, count);
    }

    fn render_browse(&mut self, area: Rect, frame: &mut ratatui::Frame) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::BORDER)
            .title(Line::from(mode_tabs(
                (Mode::Browse, false),
                self.places,
                area.width,
            )))
            // Tab goes back to whichever search was used last, so the hint
            // names it rather than leaving the reader to remember.
            .title_bottom(footer_line(
                self.notice.as_deref(),
                &fit_hints(
                    &[
                        &format!("Tab: {}", self.search_mode.label()),
                        "S-Tab: tree",
                        "^Space: keys",
                        "Enter: cd",
                        "Left: up",
                        "Right: enter",
                        "Ctrl/Alt-Left/Right: history",
                        "^P: actions",
                        "^B: pin",
                    ],
                    area.width.saturating_sub(2) as usize,
                ),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let [prompt_area, info_area, header_area, columns_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);

        // Built before the listing is borrowed, and drawn into the row the mode
        // tabs vacated when they moved to the border.
        self.mouse_header = Rect::new(area.x, area.y, area.width, 1);
        self.mouse_modes_x = area.x + 2;
        let location = self.browse_location(header_area);

        let root = self.root.clone();
        let browser = self.browser.get_or_insert_with(|| Browser::new(root));
        let filter = browser.filter.clone();
        // What the filter applies to is the folder named on the path row just
        // below, so repeating it here only crowded the line being typed on.
        let prompt = Paragraph::new(Line::from(vec![
            Span::styled("> ", theme::PROMPT),
            Span::raw(filter.as_str()),
        ]));
        frame.render_widget(prompt, prompt_area);
        if prompt_area.width > 0 && prompt_area.height > 0 {
            frame.set_cursor_position((
                prompt_area.x
                    + (2 + filter.width()).min(prompt_area.width.saturating_sub(1) as usize) as u16,
                prompt_area.y,
            ));
        }

        let shown = browser.rows().len();
        let total = browser.items().len();
        let counts = format!("  {shown}/{total} ");
        let rule_width = (info_area.width as usize).saturating_sub(counts.chars().count());
        let info = Paragraph::new(Line::from(vec![
            Span::styled(counts, theme::INFO),
            Span::styled("─".repeat(rule_width), theme::BORDER),
        ]));
        frame.render_widget(info, info_area);
        frame.render_widget(Paragraph::new(Line::from(location)), header_area);

        // The middle column is the subject, so it gets the most room.
        let [parent_area, sep1, current_area, sep2, preview_area] = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Length(1),
            Constraint::Percentage(45),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(columns_area);
        for sep in [sep1, sep2] {
            let bar: Vec<Line> = (0..sep.height)
                .map(|_| Line::from(Span::styled("│", theme::BORDER)))
                .collect();
            frame.render_widget(Paragraph::new(bar), sep);
        }

        // Parent column: the directory we are in is marked. Above the top of a
        // drive it lists the drives.
        let parent_dir = browser.parent_dir();
        if let Some((items, here)) = browser.parent_listing() {
            let height = parent_area.height as usize;
            let first = centred_scroll(here.unwrap_or(0), items.len(), height);
            let rows: Vec<ListItem> = items
                .iter()
                .enumerate()
                .skip(first)
                .take(height)
                .map(|(i, item)| {
                    let current = Some(i) == here;
                    browse_row(
                        &item.label(),
                        item.is_dir,
                        current,
                        true,
                        &[],
                        parent_area.width as usize,
                        RowMarks {
                            icons: self.config.icons,
                            favorite: item.is_dir
                                && parent_dir.as_ref().is_some_and(|parent| {
                                    self.pinned.contains(&parent.join(&item.name))
                                }),
                            // This row is the directory the middle column is
                            // listing, and the only one on screen truly open.
                            inside: current,
                        },
                    )
                })
                .collect();
            if let Some(parent) = &parent_dir {
                for (row, item) in items.iter().skip(first).take(height).enumerate() {
                    self.mouse_paths.push((
                        Rect::new(
                            parent_area.x,
                            parent_area.y + row as u16,
                            parent_area.width,
                            1,
                        ),
                        parent.join(&item.name),
                    ));
                }
            }
            frame.render_widget(List::new(rows), parent_area);
        }

        // Current column: filtered rows with match highlights.
        let height = current_area.height as usize;
        let selected = browser.selected;
        browser.first = scroll_window(browser.first, selected, shown, height);
        let first = browser.first;
        let rows: Vec<ListItem> = browser
            .rows()
            .iter()
            .enumerate()
            .skip(first)
            .take(height)
            .map(|(i, row)| {
                let item = &browser.items()[row.index];
                browse_row(
                    &item.label(),
                    item.is_dir,
                    i == selected,
                    false,
                    &row.hits,
                    current_area.width as usize,
                    RowMarks {
                        icons: self.config.icons,
                        favorite: item.is_dir
                            && self.pinned.contains(&browser.cwd.join(&item.name)),
                        inside: false,
                    },
                )
            })
            .collect();
        frame.render_widget(List::new(rows), current_area);
        self.mouse_rows = (current_area, first, shown);
        // The separator to its right is the track, for the same reason as
        // in the search list.
        if sep2.width > 0 {
            draw_scroll_thumb(
                frame.buffer_mut(),
                sep2.x,
                current_area.y,
                current_area.height,
                first,
                shown,
            );
        }

        // Preview column: contents of the selected directory. The listing is
        // loaded in the update phase, so a burst of scroll events costs no
        // directory reads; until it arrives the column is simply blank.
        let dir = browser.selected_path().filter(|p| p.is_dir());
        if let Some(dir) = dir
            && let Some((_, names)) = self.preview.as_ref().filter(|(p, _)| p == &dir)
        {
            {
                for (row, name) in names.iter().take(preview_area.height as usize).enumerate() {
                    self.mouse_paths.push((
                        Rect::new(
                            preview_area.x,
                            preview_area.y + row as u16,
                            preview_area.width,
                            1,
                        ),
                        dir.join(name),
                    ));
                }
                let rows: Vec<ListItem> = names
                    .iter()
                    .take(preview_area.height as usize)
                    .map(|n| {
                        let is_dir = n.ends_with(std::path::MAIN_SEPARATOR);
                        browse_row(
                            n,
                            is_dir,
                            false,
                            true,
                            &[],
                            preview_area.width as usize,
                            RowMarks {
                                icons: self.config.icons,
                                favorite: is_dir && self.pinned.contains(&dir.join(n)),
                                inside: false,
                            },
                        )
                    })
                    .collect();
                frame.render_widget(List::new(rows), preview_area);
            }
        }
    }

    /// The directory whose contents the preview pane should show.
    fn preview_target(&mut self) -> Option<PathBuf> {
        if self.in_tree() {
            return self.shown_tree().selected_path().filter(|p| p.is_dir());
        }
        if self.mode == Mode::Browse {
            return self.browser().selected_path().filter(|p| p.is_dir());
        }
        let mode = self.mode;
        self.source()
            .selected_entry()
            .map(|entry| entry.output_path(mode))
            .filter(|path| path.is_dir())
    }

    /// Loads the listing behind the preview pane.
    ///
    /// Called from the update phase and never from the drawing code, because
    /// reading a directory is the only blocking call on that path: doing it per
    /// frame made a fast scroll queue one directory read per notch, and the
    /// picker stopped answering the keyboard until the queue drained.
    /// Says whether the listing changed, so the caller knows to redraw.
    fn refresh_preview(&mut self) -> bool {
        match self.preview_target() {
            Some(dir) => {
                if self.preview.as_ref().is_some_and(|(p, _)| p == &dir) {
                    return false;
                }
                self.preview = Some((dir.clone(), list_dir(&dir)));
                true
            }
            None => self.preview.take().is_some(),
        }
    }

    /// The list of recent places or favorites, floating at the bottom right
    /// as the other panels do, with a prompt of its own.
    fn render_places(&mut self, mode: Mode, area: Rect, frame: &mut ratatui::Frame) {
        self.source_for(mode);
        let query = self.places_query.clone();
        let icons = self.config.icons;
        let source = self.sources[mode_index(mode)].as_mut().expect("started");
        let snapshot = source.matcher.snapshot();
        let count = snapshot.matched_item_count();
        let total = snapshot.item_count();
        let scanning = !source.scan_done.load(Ordering::Acquire);
        // Nearly the whole width: the rows are paths, and the end of a path
        // is what tells places apart. The screen under it still shows its
        // prompt and where it is, which is all the list needs of it.
        let width = (area.width * 9 / 10).clamp(40.min(area.width), area.width.saturating_sub(4));
        // Tall enough for what there is, up to two fifths of the screen, and
        // never so short that an empty list has no room for its message.
        let rows = (count.max(1) as u16).min(area.height * 2 / 5).max(3);
        let height = rows + 4;
        let (title, footer) = match mode {
            Mode::Favorites => (
                " Favorites ",
                " Enter: cd  Right: go  ^B: unpin  ^N: name  Esc ",
            ),
            _ => (" Recent ", " Enter: cd  Right: go  ^B: pin  Esc "),
        };
        let panel = crate::action_menu::float(area, width, height);
        let inner = crate::action_menu::open_panel(panel, frame, title, footer);
        let [prompt_area, info_area, rows_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(inner);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("> ", theme::PROMPT),
                Span::raw(query.clone()),
            ])),
            prompt_area,
        );
        frame.set_cursor_position((
            prompt_area.x + 2 + query.chars().count() as u16,
            prompt_area.y,
        ));
        let counts = format!(" {count}/{total} ");
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(counts.clone(), theme::INFO),
                Span::styled(
                    "─".repeat((info_area.width as usize).saturating_sub(counts.width())),
                    theme::BORDER,
                ),
            ])),
            info_area,
        );
        self.mouse_places = (rows_area, 0, 0);
        if let Some(error) = &source.scan_error {
            frame.render_widget(
                Paragraph::new(format!("{error}\nF5: retry"))
                    .style(Style::default().fg(Color::Red))
                    .wrap(Wrap { trim: false }),
                rows_area,
            );
            return;
        }
        if !scanning && total == 0 {
            let text = match mode {
                Mode::Favorites => {
                    "No favorites yet. Close this and press Ctrl-B on a folder to pin it."
                }
                _ => "No history yet.",
            };
            frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), rows_area);
            return;
        }
        let height = rows_area.height as u32;
        if height == 0 || count == 0 {
            return;
        }
        let selected = source.selected;
        source.first = scroll_window(
            source.first,
            selected as usize,
            count as usize,
            height as usize,
        );
        let first = source.first as u32;
        let last = (first + height).min(count);
        self.mouse_places = (rows_area, first as usize, count as usize);
        let pattern = snapshot.pattern().column_pattern(0);
        let mut indices = Vec::new();
        let items: Vec<ListItem> = (first..last)
            .filter_map(|idx| snapshot.get_matched_item(idx).map(|item| (idx, item)))
            .map(|(idx, item)| {
                indices.clear();
                pattern.indices(
                    item.matcher_columns[0].slice(..),
                    &mut self.highlighter,
                    &mut indices,
                );
                indices.sort_unstable();
                indices.dedup();
                let current = idx == selected;
                let marks = RowMarks {
                    icons,
                    favorite: self.pinned.contains(&item.data.path()),
                    inside: false,
                };
                let icon = marks.prefix(&item.data.display, true);
                let room = (rows_area.width as usize).saturating_sub(2 + icon.width());
                let (shown, hits) = keep_the_end(&item.data.display, &indices, room);
                let mut line = highlight_line(current, &shown, &hits);
                if !icon.is_empty() {
                    line.spans.insert(1, icons::span(icon));
                }
                if current {
                    line = line.style(theme::CURRENT);
                }
                ListItem::new(line)
            })
            .collect();
        frame.render_widget(List::new(items), rows_area);
    }

    /// Lists the directory the current selection would cd into.
    fn render_preview(&mut self, area: Rect, frame: &mut ratatui::Frame, dir: Option<&Path>) {
        let title = match dir {
            Some(d) => {
                let shown = d.strip_prefix(&self.root).unwrap_or(d);
                let shown = shown.display().to_string();
                format!(" {} ", if shown.is_empty() { "." } else { &shown })
            }
            None => String::new(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::BORDER)
            .title(Span::styled(title, theme::HEADER));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(dir) = dir else {
            return;
        };
        // Loaded in the update phase; blank for a frame while scrolling fast.
        let Some((_, names)) = self.preview.as_ref().filter(|(p, _)| p == dir) else {
            return;
        };
        for (row, name) in names.iter().take(inner.height as usize).enumerate() {
            self.mouse_paths.push((
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                dir.join(name),
            ));
        }
        let items: Vec<ListItem> = names
            .iter()
            .take(inner.height as usize)
            .map(|n| {
                let style = if n.ends_with(std::path::MAIN_SEPARATOR) {
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let is_dir = n.ends_with(std::path::MAIN_SEPARATOR);
                let marks = RowMarks {
                    icons: self.config.icons,
                    favorite: is_dir && self.pinned.contains(&dir.join(n)),
                    inside: false,
                };
                let icon = marks.prefix(n, is_dir);
                ListItem::new(
                    Line::from(vec![icons::span(icon), Span::raw(n.as_str())]).style(style),
                )
            })
            .collect();
        frame.render_widget(List::new(items), inner);
    }
}

/// Trims `text` to `width` terminal columns, ending in an ellipsis when it does
/// not fit. Counts display width, so a Japanese name is not cut mid-cell.
fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// A favorite marker takes the icon slot, including when Nerd Font icons are disabled.
#[derive(Default, Clone, Copy)]
struct RowMarks {
    icons: bool,
    favorite: bool,
    /// The row stands for the directory the listing is inside, which is the
    /// only folder on screen that is actually open.
    inside: bool,
}

impl RowMarks {
    fn prefix(self, name: &str, is_dir: bool) -> &'static str {
        if self.favorite {
            "★ "
        } else if self.inside && is_dir && self.icons {
            icons::OPEN_FOLDER
        } else {
            icons::prefix(name, is_dir, self.icons)
        }
    }
}

/// One row of a browse column, trimmed to width. Side columns are drawn muted.
fn browse_row(
    label: &str,
    is_dir: bool,
    current: bool,
    side: bool,
    hits: &[u32],
    width: usize,
    marks: RowMarks,
) -> ListItem<'static> {
    let icon = marks.prefix(label, is_dir);
    let icon = if width >= 2 + icon.width() { icon } else { "" };
    let text = fit(label, width.saturating_sub(2 + icon.width()));
    let len = text.chars().count() as u32;
    let kept: Vec<u32> = hits.iter().copied().filter(|&i| i < len).collect();
    // Only the focused column gets a background and a pointer. A side column
    // still marks where the middle one sits, but quietly, so which list the
    // cursor is in can be read at a glance.
    let focused = current && !side;
    let style = match (current, side, is_dir) {
        (true, false, _) => theme::CURRENT,
        (true, true, _) => theme::HERE,
        (_, true, true) => theme::SIDE_DIR,
        (_, false, true) => theme::DIR,
        (_, true, false) => theme::SIDE,
        (_, false, false) => Style::default(),
    };
    let mut line = highlight_line(focused, &text, &kept);
    if !icon.is_empty() {
        line.spans.insert(1, icons::span(icon));
    }
    ListItem::new(line.style(style))
}

/// Key hints for the bottom border, most important first, keeping as many
/// whole hints as fit in `width` columns.
///
/// The border draws them right-aligned and cuts whatever does not fit from
/// the left, which is where Tab is. Dropping the least used hints instead
/// keeps the keys that move between screens on a narrow terminal.
fn fit_hints(hints: &[&str], width: usize) -> String {
    let mut kept = hints.len();
    loop {
        let line = format!(" {} ", hints[..kept].join("  "));
        if kept <= 1 || line.width() <= width {
            return line;
        }
        kept -= 1;
    }
}

/// The bottom border: a notice when there is one, otherwise the key hints.
///
/// A notice is drawn in its own colour so it cannot be mistaken for the hints
/// that are always there, and failures are marked apart from confirmations.
fn footer_line(notice: Option<&str>, hints: &str) -> Line<'static> {
    match notice {
        Some(text) => {
            let failed = text.starts_with("Cannot") || text.starts_with("No ");
            let style = if failed {
                theme::NOTICE_BAD
            } else {
                theme::NOTICE
            };
            Line::from(Span::styled(format!(" {text} "), style)).right_aligned()
        }
        None => Line::from(Span::styled(hints.to_string(), theme::BORDER)).right_aligned(),
    }
}

/// The row to draw first, given where the last frame started.
///
/// The window only moves when the selection would fall outside it, which is
/// how a list is normally read: the cursor travels to the edge and the rows
/// start moving only once it is there. Recomputing the start from the
/// selection alone glued the selection to the bottom row for good, once the
/// list was longer than the screen.
fn scroll_window(first: usize, selected: usize, count: usize, height: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let first = first.min(count.saturating_sub(height));
    if selected < first {
        selected
    } else if selected >= first + height {
        selected + 1 - height
    } else {
        first
    }
}

/// First visible row that puts `focus` in the middle of a `height`-tall window.
///
/// The parent column is context, so the folders either side of the current one
/// matter as much as the row itself; pinning it to the last line hid them and
/// left the marker against the bottom edge. Near the ends of the list the
/// window stops rather than scrolling past them.
fn centred_scroll(focus: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    focus
        .saturating_sub(height / 2)
        .min(len.saturating_sub(height))
}

/// Where a scrollbar's thumb goes, as its first row and its length, or
/// `None` when every row already fits.
///
/// The thumb reaches an end of the track only when the window reaches that
/// end of the list. Rounding alone parks it on the last row while rows are
/// still hidden below, which is the one thing a scrollbar must not say.
fn scroll_thumb(first: usize, count: usize, height: usize) -> Option<(usize, usize)> {
    if height == 0 || count <= height {
        return None;
    }
    let len = (height * height / count).max(1);
    let travel = height - len;
    let last = count - height;
    let first = first.min(last);
    let start = if first == 0 {
        0
    } else if first == last {
        travel
    } else {
        let start = (first * travel + last / 2) / last;
        // A track too short to leave a row between its ends has nowhere
        // else to put a position in the middle.
        if travel >= 2 {
            start.clamp(1, travel - 1)
        } else {
            start
        }
    };
    Some((start, len))
}

/// Draws the thumb over a column that already holds a vertical line, so the
/// line is the track and no row gives up a column of width to it.
fn draw_scroll_thumb(
    buffer: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    height: u16,
    first: usize,
    count: usize,
) {
    let Some((start, len)) = scroll_thumb(first, count, height as usize) else {
        return;
    };
    for row in start..start + len {
        if let Some(cell) = buffer.cell_mut((x, y + row as u16)) {
            cell.set_symbol("┃").set_style(theme::SCROLL);
        }
    }
}

/// What browse should be showing when it lines up with the search, beyond
/// the folder being searched itself.
///
/// Only a list of that folder can say. recent and favorites hold places of
/// their own, and Tab passes through both on the way to browse, which opened
/// it on whichever of them the cursor had stopped over.
fn browse_reveal(leaving: Mode, selected: Option<Entry>) -> Option<PathBuf> {
    selected
        .filter(|_| matches!(leaving, Mode::Dirs | Mode::Files))
        .map(|entry| entry.path())
        .filter(|path| path.exists())
}

/// The three buttons, greyed out where there is nowhere to go.
fn nav_buttons(area: Rect, available: [bool; 3]) -> (Vec<Span<'static>>, Vec<(Rect, Nav)>, u16) {
    let limit = area.right();
    let mut x = area.x;
    let mut spans = Vec::new();
    let mut targets = Vec::new();
    for ((nav, glyph), enabled) in Nav::BUTTONS.iter().zip(available) {
        spans.push(Span::styled(
            *glyph,
            if enabled {
                theme::HEADER
            } else {
                theme::BORDER
            },
        ));
        if enabled && x + Nav::WIDTH <= limit {
            targets.push((Rect::new(x, area.y, Nav::WIDTH, 1), *nav));
        }
        x += Nav::WIDTH;
    }
    spans.push(Span::raw(" "));
    (spans, targets, x + 1)
}

/// The scan root as a breadcrumb, paired with where each step above the
/// current one can be clicked. The last segment is bold, so the folder a
/// search covers can be found in the header at a glance.
fn search_location(path: &Path, area: Rect) -> (Vec<Span<'static>>, Vec<(Rect, PathBuf)>) {
    let limit = area.x + area.width;
    let mut x = area.x;
    let mut spans = Vec::new();
    let mut targets = Vec::new();
    for (span, dir) in crumb_spans(path) {
        let width = span.content.width() as u16;
        // A path too long for the row is clipped, and the part that is not
        // drawn must not answer clicks.
        if let Some(dir) = dir
            && x + width <= limit
        {
            targets.push((Rect::new(x, area.y, width, 1), dir));
        }
        x += width;
        spans.push(span);
    }
    (spans, targets)
}

/// Each step of `path` paired with the directory it stands for, root first.
fn breadcrumb(path: &Path) -> Vec<(String, PathBuf)> {
    let mut dirs: Vec<&Path> = path.ancestors().collect();
    dirs.reverse();
    dirs.into_iter()
        .map(|dir| {
            let text = match dir.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                // The root has no file name, so it prints whole: "C:\" or "/".
                None => dir.display().to_string(),
            };
            (text, dir.to_path_buf())
        })
        .collect()
}

/// The path as spans, each paired with the directory it leads to.
///
/// The trailing segment is the one being looked at, so it is white and bold
/// while the trail above it stays grey; a path that reads as one flat string
/// is easy to overlook next to the mode tabs.
fn crumb_spans(path: &Path) -> Vec<(Span<'static>, Option<PathBuf>)> {
    let crumbs = breadcrumb(path);
    let last = crumbs.len().saturating_sub(1);
    let mut spans: Vec<(Span<'static>, Option<PathBuf>)> = Vec::new();
    for (index, (text, dir)) in crumbs.into_iter().enumerate() {
        if index > 0
            && !spans
                .last()
                .is_some_and(|(span, _)| span.content.ends_with(std::path::MAIN_SEPARATOR))
        {
            spans.push((
                Span::styled(std::path::MAIN_SEPARATOR.to_string(), theme::TRAIL),
                None,
            ));
        }
        let style = if index == last {
            theme::HERE_PATH
        } else {
            theme::TRAIL
        };
        spans.push((Span::styled(text, style), Some(dir)));
    }
    spans
}

/// A tab on the top border: a mode, and for browse which way it is drawn,
/// true for the tree.
type Tab = (Mode, bool);

/// Every search, then browse as columns and as a tree. The tree is a tab of
/// its own so that it can be seen, and reached, without the key panel.
fn tabs() -> Vec<Tab> {
    MODE_ORDER
        .iter()
        .filter(|m| !matches!(m, Mode::Recent | Mode::Favorites))
        .map(|&m| (m, false))
        .chain([(Mode::Browse, true)])
        .collect()
}

fn tab_label((mode, tree): Tab) -> &'static str {
    if tree { "tree" } else { mode.label() }
}

/// What is drawn before a tab's label, after the one before it.
///
/// The two searches share a bracket and browse has its own: Shift-Tab steps
/// within a bracket and Tab goes across, and four tabs in one row would read
/// as four of the same kind.
fn tab_separator(tab: Tab) -> &'static str {
    if tab == (Mode::Browse, false) {
        "]  ["
    } else {
        "|"
    }
}

/// Where each tab's label starts and ends, counted in columns from the first
/// label. Drawing and clicking both go by it, so a click cannot land on a
/// different tab from the one drawn there.
fn tab_offsets() -> Vec<(Tab, u16, u16)> {
    let mut offsets = Vec::new();
    let mut x = 0;
    for (i, tab) in tabs().into_iter().enumerate() {
        if i > 0 {
            x += tab_separator(tab).len() as u16;
        }
        let end = x + tab_label(tab).len() as u16;
        offsets.push((tab, x, end));
        x = end;
    }
    offsets
}

/// The lists that open over the screen, as buttons after the tabs, each
/// with its icon: the star the rows mark favorites with, and a clock for the
/// recent places. They sit outside the brackets so they read
/// as lists rather than as more tabs. Ctrl-S and Ctrl-R do the same from the
/// keyboard. The icons are plain Unicode, so they show without a Nerd Font.
const PLACE_BUTTONS: [(Mode, &str, &str); 2] = [
    (Mode::Favorites, "★ ", "favorites"),
    (Mode::Recent, "◷ ", "recent"),
];

/// Where each button starts and ends, counted in columns from the first tab
/// label, leaving out any that would run into the border's corner at `width`.
/// Drawing and clicking both go by it. The buttons are the last thing on the
/// border, so a narrow screen loses them and keeps the tabs.
fn place_buttons(width: u16) -> Vec<(Mode, u16, u16)> {
    // After the closing bracket and a gap the width of the one between tabs.
    let mut x = tab_offsets().last().map_or(0, |&(_, _, end)| end) + 3;
    let mut buttons = Vec::new();
    for (i, (mode, icon, label)) in PLACE_BUTTONS.into_iter().enumerate() {
        if i > 0 {
            x += 2;
        }
        let end = x + (icon.width() + label.width()) as u16;
        // The first label sits at column 2 of the box, and the corner takes
        // the last column.
        if end + 3 > width {
            break;
        }
        buttons.push((mode, x, end));
        x = end;
    }
    buttons
}

/// The tabs, `[dirs|files]  [browse|tree]`, drawn on the top border with the
/// one shown underlined, then the buttons for the lists of places with the
/// open one underlined. Recent and favorites are not screens but lists that
/// open over one, so they are buttons rather than tabs.
fn mode_tabs(shown: Tab, open: Option<Mode>, width: u16) -> Vec<Span<'static>> {
    let mut header: Vec<Span> = vec![Span::styled("[", theme::HEADER)];
    for (i, tab) in tabs().into_iter().enumerate() {
        if i > 0 {
            header.push(Span::styled(tab_separator(tab), theme::HEADER));
        }
        let style = if tab == shown {
            theme::HEADER.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme::HEADER
        };
        header.push(Span::styled(tab_label(tab), style));
    }
    header.push(Span::styled("]", theme::HEADER));
    for (mode, ..) in place_buttons(width) {
        header.push(Span::styled("  ", theme::HEADER));
        let (_, icon, label) = PLACE_BUTTONS
            .into_iter()
            .find(|&(m, ..)| m == mode)
            .expect("a drawn button is one of the two");
        let style = if open == Some(mode) {
            theme::HEADER.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme::HEADER
        };
        header.push(Span::styled(icon, theme::HEADER));
        header.push(Span::styled(label, style));
    }
    header.push(Span::styled(" ", theme::HEADER));
    header
}

/// Directory names first (with a trailing separator), then files, both sorted case-insensitively.
fn list_dir(dir: &Path) -> Vec<String> {
    if std::fs::read_dir(dir).is_err() {
        return vec!["(unreadable)".to_string()];
    }
    browse::read_dir(dir)
        .iter()
        .take(PREVIEW_LIMIT)
        .map(browse::Item::label)
        .collect()
}

/// Colours from fzf's default dark theme, by 256-colour index, so the picker
/// looks like the fzf-based commands it replaces.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    pub const BORDER: Style = Style::new().fg(Color::Indexed(240));
    /// The scrollbar's thumb, drawn over a border line. A heavier line in a
    /// lighter grey, so it reads by shape as well as by colour.
    pub const SCROLL: Style = Style::new().fg(Color::Indexed(248));
    pub const PROMPT: Style = Style::new().fg(Color::Indexed(110));
    pub const INFO: Style = Style::new().fg(Color::Indexed(144));
    pub const HEADER: Style = Style::new().fg(Color::Indexed(109));
    pub const POINTER: Style = Style::new().fg(Color::Indexed(161));
    pub const MATCH: Style = Style::new().fg(Color::Indexed(108));
    /// Directories in the column being worked in. Bright enough to read on a
    /// black background, which the terminal's own blue is not.
    pub const DIR: Style = Style::new().fg(Color::Indexed(75));
    /// The side columns are context, not the subject, so they are muted and
    /// the eye lands on the middle column.
    pub const SIDE: Style = Style::new().fg(Color::Indexed(244));
    pub const SIDE_DIR: Style = Style::new().fg(Color::Indexed(67));
    /// Marks the middle column's directory inside a side column. It borrows the
    /// pointer's colour so it reads as related to the cursor, and stands out
    /// against a column of blue folders; the background and the bar stay with
    /// the focused column, which is what says where the cursor actually is.
    pub const HERE: Style = Style::new()
        .fg(Color::Indexed(161))
        .add_modifier(Modifier::BOLD);
    /// The ancestors in the header path, kept quiet so the folder in view reads
    /// as the subject rather than as more of the mode tabs beside it.
    pub const TRAIL: Style = Style::new().fg(Color::Indexed(244));
    /// A message about something that just happened. The key hints it replaces
    /// are permanent furniture drawn in the border colour, so a notice left in
    /// that colour reads as furniture too and goes unnoticed.
    pub const NOTICE: Style = Style::new()
        .fg(Color::Indexed(109))
        .add_modifier(Modifier::BOLD);
    pub const NOTICE_BAD: Style = Style::new()
        .fg(Color::Indexed(161))
        .add_modifier(Modifier::BOLD);
    pub const HERE_PATH: Style = Style::new()
        .fg(Color::Indexed(255))
        .add_modifier(Modifier::BOLD);
    pub const CURRENT: Style = Style::new()
        .fg(Color::Indexed(255))
        .bg(Color::Indexed(236))
        .add_modifier(Modifier::BOLD);
}

/// Shown next to the counts while a scan is still feeding the list.
const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

/// Builds one row, colouring the characters at `indices` (sorted, char positions).
/// The current row gets fzf's pointer bar in the gutter.
fn highlight_line(current: bool, text: &str, indices: &[u32]) -> Line<'static> {
    let pointer = if current {
        Span::styled("▌ ", theme::POINTER)
    } else {
        Span::raw("  ")
    };
    let mut spans = vec![pointer];
    spans.extend(match_spans(text, indices));
    Line::from(spans)
}

/// `text` in runs, the characters at `indices` (sorted, char positions) coloured.
fn match_spans(text: &str, indices: &[u32]) -> Vec<Span<'static>> {
    let matched = theme::MATCH;
    let mut spans = Vec::new();
    let mut next = indices.iter().peekable();
    let mut run_start = 0;
    let mut run_hit = false;
    for (pos, (byte, _)) in text.char_indices().enumerate() {
        let hit = next.peek().is_some_and(|&&i| i as usize == pos);
        if hit {
            next.next();
        }
        if hit != run_hit && byte > run_start {
            let style = if run_hit { matched } else { Style::default() };
            spans.push(Span::styled(text[run_start..byte].to_string(), style));
            run_start = byte;
        }
        run_hit = hit;
    }
    if run_start < text.len() {
        let style = if run_hit { matched } else { Style::default() };
        spans.push(Span::styled(text[run_start..].to_string(), style));
    }
    spans
}

/// `text` cut to `width` columns from the front, since the end of a path is
/// what tells places apart, with `hits` (sorted char positions) moved to
/// match. A favorite's name, the `:name` before the two spaces, is kept
/// whole and the path after it is what gets cut.
fn keep_the_end(text: &str, hits: &[u32], width: usize) -> (String, Vec<u32>) {
    if text.width() <= width {
        return (text.to_string(), hits.to_vec());
    }
    let (head, rest) = match text.strip_prefix(':').and_then(|_| text.find("  ")) {
        Some(at) => text.split_at(at + 2),
        None => ("", text),
    };
    let head_chars = head.chars().count();
    let room = width.saturating_sub(head.width());
    // Kept from the end, leaving one column for the ellipsis.
    let mut kept = 0;
    let mut used = 1;
    for ch in rest.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        kept += 1;
    }
    let rest_chars = rest.chars().count();
    let dropped = rest_chars - kept;
    let tail: String = rest.chars().skip(dropped).collect();
    let shown = format!("{head}…{tail}");
    // Positions in the head stay; in the tail they move left by the chars
    // dropped, less the one the ellipsis takes; in between they are gone.
    let moved = hits
        .iter()
        .filter_map(|&i| {
            let i = i as usize;
            if i < head_chars {
                Some(i as u32)
            } else if i >= head_chars + dropped {
                Some((i - dropped + 1) as u32)
            } else {
                None
            }
        })
        .collect();
    (shown, moved)
}

/// Shares out the matched characters of `display`, a path relative to the
/// tree's root, among the rows that show it: each folder on the way and the
/// match itself, as positions in that row's name. A row keeps its own match
/// over one passing through it, and otherwise the best match that reaches it.
fn spread_marks(
    path: &Path,
    display: &str,
    indices: &[u32],
    marks: &mut HashMap<PathBuf, Vec<u32>>,
) {
    let names: Vec<&str> = display.split(std::path::is_separator).collect();
    let mut row = path.to_path_buf();
    // From the name at the end back to the first folder, so `row` climbs
    // along with the names.
    let mut end = display.chars().count() as u32;
    for (k, name) in names.iter().enumerate().rev() {
        let start = end - name.chars().count() as u32;
        let own: Vec<u32> = indices
            .iter()
            .filter(|&&i| i >= start && i < end)
            .map(|&i| i - start)
            .collect();
        if k == names.len() - 1 {
            marks.insert(row.clone(), own);
        } else if !own.is_empty() {
            marks.entry(row.clone()).or_insert(own);
        }
        end = start.saturating_sub(1);
        if !row.pop() {
            break;
        }
    }
}

/// The picker draws on stderr so stdout stays clean for the selected path.
/// Keep command output separate from the inline picker and shell history.
struct ActionScreen;

impl ActionScreen {
    fn enter() -> Result<Self, Error> {
        crossterm::execute!(io::stderr(), crossterm::terminal::EnterAlternateScreen)?;
        let screen = Self;
        crossterm::execute!(
            io::stderr(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            crossterm::cursor::Show
        )?;
        Ok(screen)
    }
}

impl Drop for ActionScreen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stderr(), crossterm::terminal::LeaveAlternateScreen);
    }
}

fn wait_for_return() -> Result<(), Error> {
    enable_raw_mode()?;
    let result = (|| -> Result<(), Error> {
        loop {
            if let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && (matches!(key.code, KeyCode::Enter | KeyCode::Esc)
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
            {
                return Ok(());
            }
        }
    })();
    let restored = disable_raw_mode();
    result?;
    restored?;
    Ok(())
}

/// Owns raw mode and the alternate screen, including error cleanup.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
    mouse: bool,
}

impl TerminalGuard {
    /// Takes the whole window on the terminal's alternate screen, as fzf does
    /// without --height, so the listing gets every row and the shell's screen
    /// comes back untouched on exit. Drawn inline over the shell's screen
    /// instead, the rows it scrolled away counted as the output of the `c`
    /// command, and a terminal that pins the command line of long output to
    /// its top edge, as the one in VS Code does, covered the tabs and prompt.
    fn enter(mouse: bool) -> Result<Self, Error> {
        enable_raw_mode()?;
        match Self::open() {
            Ok(terminal) => {
                let guard = Self { terminal, mouse };
                if mouse {
                    crossterm::execute!(io::stderr(), EnableMouseCapture)?;
                }
                Ok(guard)
            }
            Err(err) => {
                let _ = disable_raw_mode();
                Err(err)
            }
        }
    }

    fn open() -> Result<Terminal<CrosstermBackend<io::Stderr>>, Error> {
        crossterm::execute!(io::stderr(), crossterm::terminal::EnterAlternateScreen)?;
        let terminal = Terminal::with_options(
            CrosstermBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        );
        match terminal {
            Ok(mut terminal) => {
                terminal.clear()?;
                Ok(terminal)
            }
            Err(err) => {
                let _ =
                    crossterm::execute!(io::stderr(), crossterm::terminal::LeaveAlternateScreen);
                Err(err.into())
            }
        }
    }

    /// Clears the screen after the window changed size; the next draw takes
    /// the new size. Cells outside the old area would otherwise keep whatever
    /// the terminal put there.
    fn reopen(&mut self) -> Result<(), Error> {
        self.terminal.clear()?;
        Ok(())
    }
}

impl std::ops::Deref for TerminalGuard {
    type Target = Terminal<CrosstermBackend<io::Stderr>>;
    fn deref(&self) -> &Self::Target {
        &self.terminal
    }
}

impl std::ops::DerefMut for TerminalGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.mouse {
            let _ = crossterm::execute!(io::stderr(), DisableMouseCapture);
        }
        // Back to the shell's screen, with the prompt where the picker opened.
        let _ = crossterm::execute!(io::stderr(), crossterm::terminal::LeaveAlternateScreen);
        let _ = disable_raw_mode();
        let _ = io::stderr().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorite_marker_stays_yellow_and_preserves_matches_without_nerd_icons() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;
        for icons in [false, true] {
            for current in [false, true] {
                let area = Rect::new(0, 0, 24, 1);
                let mut buffer = Buffer::empty(area);
                let marks = RowMarks {
                    icons,
                    favorite: true,
                    inside: false,
                };
                let item = browse_row("日本語", true, current, false, &[1], 24, marks);
                List::new(vec![item]).render(area, &mut buffer);
                assert_eq!(buffer[(2, 0)].symbol(), "★");
                assert_eq!(buffer[(2, 0)].fg, Color::Indexed(220));
                let name_x = 2 + "★ ".width() as u16;
                assert_eq!(buffer[(name_x, 0)].symbol(), "日");
                assert_eq!(buffer[(name_x + 2, 0)].symbol(), "本");
                assert_eq!(buffer[(name_x + 2, 0)].fg, theme::MATCH.fg.unwrap());
                if current {
                    assert_eq!(buffer[(2, 0)].bg, theme::CURRENT.bg.unwrap());
                }
                for width in 2..12 {
                    let mut buffer = Buffer::empty(area);
                    List::new(vec![browse_row(
                        "日本語の名前",
                        true,
                        current,
                        false,
                        &[],
                        width,
                        marks,
                    )])
                    .render(area, &mut buffer);
                    for x in width as u16..area.width {
                        assert_eq!(buffer[(x, 0)].symbol(), " ");
                    }
                }
            }
        }
    }

    fn test_picker(root: PathBuf, mode: Mode) -> Picker {
        Picker {
            sources: std::array::from_fn(|_| None),
            browser: None,
            mode,
            query: String::new(),
            root,
            config: Config::default(),
            highlighter: Matcher::new(MatchConfig::DEFAULT.match_paths()),
            preview: None,
            frame_count: 0,
            notice_until: None,
            clicks_blocked_until: None,
            notice: None,
            pinned: crate::favorites::Index::default(),
            menu: None,
            naming: None,
            keys: None,
            tree_view: false,
            tree: None,
            tree_query: String::new(),
            tree_search: None,
            tree_search_root: PathBuf::new(),
            found: None,
            found_keep: false,
            found_marks: HashMap::new(),
            tree_rows: 20,
            tree_dirs: HashMap::new(),
            places: None,
            places_query: String::new(),
            mouse_places: (Rect::default(), 0, 0),
            browser_root: PathBuf::new(),
            search_mode: first_search(mode),
            unlimited: false,
            roots_back: Vec::new(),
            roots_forward: Vec::new(),
            mouse_rows: (Rect::default(), 0, 0),
            mouse_header: Rect::default(),
            mouse_nav: Vec::new(),
            mouse_crumbs: Vec::new(),
            mouse_modes_x: 0,
            mouse_paths: Vec::new(),
            last_click: None,
            after_action: crate::actions::Then::Stay,
            after_cd: None,
            last_erase: None,
        }
    }

    #[test]
    fn double_click_enters_directory_and_filters_its_children() {
        use ratatui::backend::TestBackend;
        let root = std::env::temp_dir().join(format!("tadoru-double-click-{}", std::process::id()));
        std::fs::create_dir_all(root.join("C/ssl")).unwrap();
        std::fs::create_dir_all(root.join("C/other")).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let (area, _, _) = picker.mouse_rows;
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };
        picker.handle_mouse(click);
        assert_eq!(picker.browser().cwd, root);
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..click
        });
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        picker.handle_mouse(click);
        assert_eq!(picker.browser().cwd, root.join("C"));
        for ch in "ssl".chars() {
            picker.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(picker.browser().rows().len(), 1);
        assert_eq!(picker.browser().selected_path(), Some(root.join("C/ssl")));
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // The path row names the folder being filtered, so the prompt does
        // not repeat it.
        assert!(!screen.contains("[Filter:"), "the label came back");
        let shown = root.join("C");
        assert!(
            screen.contains(&shown.display().to_string()),
            "the path row should name the folder being filtered"
        );
        drop(picker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn switching_modes_restores_browse_location_selection_and_filter() {
        let root = std::env::temp_dir().join(format!("tadoru-mode-restore-{}", std::process::id()));
        std::fs::create_dir_all(root.join("ahktest")).unwrap();
        std::fs::create_dir_all(root.join("AWS")).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.browser().set_filter("AWS");
        let selected = picker.browser().selected_path();
        picker.toggle_browse();
        assert_eq!(picker.mode, Mode::Dirs);
        let source = picker.source();
        source.finish_scan();
        while source.matcher.tick(TICK_MS).running {}
        picker.toggle_browse();
        assert_eq!(picker.browser().cwd, root);
        assert_eq!(picker.browser().filter, "AWS");
        assert_eq!(picker.browser().selected_path(), selected);
        drop(picker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_parent_column_keeps_the_current_folder_off_the_edges() {
        // Short lists never scroll.
        assert_eq!(centred_scroll(0, 3, 10), 0);
        assert_eq!(centred_scroll(2, 3, 10), 0);
        // In a long list the row sits in the middle, with context either side.
        assert_eq!(centred_scroll(20, 100, 10), 15);
        // Near the start and the end the window stops instead of overshooting.
        assert_eq!(centred_scroll(1, 100, 10), 0);
        assert_eq!(centred_scroll(99, 100, 10), 90);
        // A zero-height column asks for nothing.
        assert_eq!(centred_scroll(5, 100, 0), 0);
    }

    #[test]
    fn only_the_directory_being_listed_gets_the_open_folder_icon() {
        let marks = |inside| RowMarks {
            icons: true,
            favorite: false,
            inside,
        };
        // The folder the middle column is listing is the one actually open.
        assert_eq!(marks(true).prefix("work", true), icons::OPEN_FOLDER);
        // Everything else, selected or not, is just a folder named in a list.
        assert_ne!(marks(false).prefix("work", true), icons::OPEN_FOLDER);
        // Files are unaffected, and a pin still wins over both.
        assert_ne!(marks(true).prefix("notes.txt", false), icons::OPEN_FOLDER);
        assert_eq!(
            RowMarks {
                icons: true,
                favorite: true,
                inside: true,
            }
            .prefix("work", true),
            "★ "
        );
    }

    #[test]
    fn only_the_focused_column_carries_the_selection_background() {
        let marks = RowMarks {
            icons: false,
            favorite: false,
            inside: false,
        };
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        // (background of the name cell, pointer glyph) as actually drawn.
        let drawn = |current: bool, side: bool| {
            let area = Rect::new(0, 0, 20, 1);
            let mut buffer = Buffer::empty(area);
            let item = browse_row("work", true, current, side, &[], 20, marks);
            List::new(vec![item]).render(area, &mut buffer);
            (
                buffer[(4, 0)].bg,
                buffer[(0, 0)].symbol().to_string(),
                buffer[(4, 0)].fg,
            )
        };

        // The middle column owns the cursor: a background and the pointer bar.
        let (bg, pointer, _) = drawn(true, false);
        assert_eq!(bg, Color::Indexed(236));
        assert_eq!(pointer, "▌");

        // A side column marks the same directory without either, so the two
        // highlighted rows on screen cannot be mistaken for each other.
        let (bg, pointer, fg) = drawn(true, true);
        assert_eq!(bg, Color::Reset);
        assert_eq!(pointer, " ");
        assert_eq!(fg, theme::HERE.fg.unwrap());

        // An ordinary side row stays muted.
        let (bg, _, fg) = drawn(false, true);
        assert_eq!(bg, Color::Reset);
        assert_eq!(fg, theme::SIDE_DIR.fg.unwrap());
    }

    #[test]
    fn a_notice_is_told_apart_from_the_permanent_key_hints() {
        const HINTS: &str = " Tab: browse  Enter: cd ";
        let style_of = |line: &Line<'static>| line.spans[0].style;

        // With nothing to say the bar is furniture, drawn like the border.
        let idle = footer_line(None, HINTS);
        assert_eq!(style_of(&idle), theme::BORDER);
        assert_eq!(idle.spans[0].content, HINTS);

        // A confirmation has to stand out from that furniture, or it reads as
        // more of it and goes unnoticed.
        let done = footer_line(Some("Opened: report.xlsx"), HINTS);
        assert_eq!(style_of(&done), theme::NOTICE);
        assert_ne!(style_of(&done), theme::BORDER);

        // A failure is marked apart from a confirmation again.
        for bad in ["Cannot open x: denied", "No available directory in history"] {
            assert_eq!(style_of(&footer_line(Some(bad), HINTS)), theme::NOTICE_BAD);
        }
    }

    #[test]
    fn a_notice_reaches_the_screen_in_its_own_colour() {
        use ratatui::backend::TestBackend;
        let root = crate::testing::temp_dir().join(format!("tadoru-notice-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();

        for (mode, expected) in [
            (Mode::Browse, theme::NOTICE_BAD),
            (Mode::Dirs, theme::NOTICE_BAD),
        ] {
            let mut picker = test_picker(root.clone(), mode);
            picker.notice = Some("Cannot open it: denied".into());
            let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
            terminal
                .draw(|frame| picker.render(Rect::new(0, 0, 90, 12), frame))
                .unwrap();

            // Find the message on the bottom border and check it is not drawn
            // in the same colour as the key hints it replaced.
            let buffer = terminal.backend().buffer();
            let bottom = 11;
            // Count in columns, not bytes: the border glyphs are multi-byte.
            let cells: Vec<&str> = (0..90).map(|x| buffer[(x, bottom)].symbol()).collect();
            let at = (0..cells.len() - 6)
                .find(|&x| cells[x..x + 6].concat() == "Cannot")
                .unwrap_or_else(|| panic!("{mode:?}: {}", cells.concat()))
                as u16;
            assert_eq!(buffer[(at, bottom)].fg, expected.fg.unwrap(), "{mode:?}");
            assert_ne!(buffer[(at, bottom)].fg, theme::BORDER.fg.unwrap());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_scan_stops_at_the_limit_and_the_screen_says_so() {
        let root = crate::testing::temp_dir().join(format!("tadoru-limit-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..40 {
            std::fs::create_dir_all(root.join(format!("folder{i:02}"))).unwrap();
        }
        let config = Config {
            scan_limit: 10,
            ..Config::default()
        };

        let mut source = Source::start(Mode::Dirs, &root, &config, config.scan_limit);
        source.finish_scan();
        while source.matcher.tick(TICK_MS).running {}
        let found = source.matcher.snapshot().item_count() as usize;
        assert!(source.truncated.load(Ordering::Acquire), "found {found}");
        // Workers stop as soon as one of them passes the ceiling, so a few
        // more can land first. What matters is that the walk ends early.
        assert!((10..40).contains(&found), "found {found}");

        // A short list must not look like the whole folder.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.config = config;
        picker.sources[mode_index(Mode::Dirs)] = Some(source);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 12)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("stopped at scan_limit"), "{screen}");

        // Without a ceiling the same folder comes back whole and says nothing.
        let plain = Config::default();
        let mut source = Source::start(Mode::Dirs, &root, &plain, plain.scan_limit);
        source.finish_scan();
        while source.matcher.tick(TICK_MS).running {}
        assert_eq!(source.matcher.snapshot().item_count(), 40);
        assert!(!source.truncated.load(Ordering::Acquire));
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn a_short_list_can_be_collected_in_full_from_the_screen() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-collect-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..40 {
            std::fs::create_dir_all(root.join(format!("folder{i:02}"))).unwrap();
        }
        let config = Config {
            scan_limit: 10,
            ..Config::default()
        };
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.config = config;
        picker.query = "folder".into();
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        assert!(picker.source().truncated.load(Ordering::Acquire));

        // Without this the only way past the ceiling is to edit a file and
        // start the picker again, in the middle of looking for something.
        picker.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        assert_eq!(picker.source().matcher.snapshot().item_count(), 40);
        assert!(!picker.source().truncated.load(Ordering::Acquire));
        // The filter is still the one that was typed before asking.
        assert_eq!(picker.query, "folder");

        // The next mode entered collects in full too, without asking again.
        picker.mode = Mode::Files;
        picker.source().finish_scan();
        assert!(!picker.source().truncated.load(Ordering::Acquire));
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn the_cursor_walks_the_window_before_the_rows_start_moving() {
        // Ten rows on screen, forty in the list.
        let (count, height) = (40, 10);
        let mut first = 0;

        // Walking down: the rows hold still until the cursor reaches the last
        // line, then move one at a time.
        for selected in 0..height {
            first = scroll_window(first, selected, count, height);
            assert_eq!(first, 0, "moved early at {selected}");
        }
        first = scroll_window(first, height, count, height);
        assert_eq!(first, 1);

        // Now at the bottom of the window. Walking back up must bring the
        // cursor up the screen, not drag the rows with it, which is what
        // recomputing the start from the selection alone used to do.
        let mut selected = height;
        for step in 1..height {
            selected -= 1;
            let before = first;
            first = scroll_window(first, selected, count, height);
            assert_eq!(first, before, "rows moved at step {step}");
        }
        // Only once the cursor is on the first line do the rows follow.
        selected -= 1;
        first = scroll_window(first, selected, count, height);
        assert_eq!(first, 0);

        // A jump lands the target on screen rather than off it.
        assert_eq!(scroll_window(0, 39, count, height), 30);
        assert_eq!(scroll_window(30, 0, count, height), 0);
        // A list that shrank pulls the start back to where rows still exist.
        assert_eq!(scroll_window(30, 2, 20, height), 2);
        // One that now fits entirely starts at the top.
        assert_eq!(scroll_window(30, 2, 5, height), 0);
        // No room to draw anything is not a panic.
        assert_eq!(scroll_window(7, 3, count, 0), 0);
    }

    #[test]
    fn the_thumb_touches_an_end_only_when_the_list_does() {
        assert_eq!(scroll_thumb(0, 10, 10), None);
        assert_eq!(scroll_thumb(0, 40, 0), None);
        for height in [2, 3, 7, 10] {
            for count in height + 1..=120 {
                let last = count - height;
                let mut previous = 0;
                for first in 0..=last {
                    let (start, len) = scroll_thumb(first, count, height).unwrap();
                    let at = format!("{first}/{count} in {height}");
                    assert!(len >= 1 && start + len <= height, "{at}");
                    assert!(start >= previous, "thumb moved back at {at}");
                    previous = start;
                    if height - len >= 2 {
                        assert_eq!(start == 0, first == 0, "{at}");
                        assert_eq!(start + len == height, first == last, "{at}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_list_longer_than_the_screen_shows_where_the_window_is() {
        use ratatui::backend::TestBackend;
        let root = crate::testing::temp_dir().join(format!("tadoru-thumb-{}", std::process::id()));
        for i in 0..40 {
            std::fs::create_dir_all(root.join(format!("folder{i:02}"))).unwrap();
        }
        // Beside the long list, not in it: dirs searches the whole tree.
        let small =
            crate::testing::temp_dir().join(format!("tadoru-thumb-small-{}", std::process::id()));
        for i in 0..3 {
            std::fs::create_dir_all(small.join(format!("inner{i}"))).unwrap();
        }
        // Where the thumb was drawn, as (column, row).
        let thumb = |picker: &mut Picker| {
            let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
            terminal
                .draw(|frame| picker.render(frame.area(), frame))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let mut cells = Vec::new();
            for y in 0..12 {
                for x in 0..90 {
                    if buffer[(x, y)].symbol() == "┃" {
                        cells.push((x, y));
                    }
                }
            }
            cells
        };
        let scanned = |picker: &mut Picker| {
            picker.source().finish_scan();
            while picker.source().matcher.tick(TICK_MS).running {}
        };
        // The rows sit below the prompt, the counts and the path, and above
        // the bottom border.
        let (top, bottom) = (4, 10);

        let mut dirs = test_picker(root.clone(), Mode::Dirs);
        scanned(&mut dirs);
        let mut browse = test_picker(root.clone(), Mode::Browse);
        for (name, picker) in [("dirs", &mut dirs), ("browse", &mut browse)] {
            let cells = thumb(picker);
            assert!(!cells.is_empty(), "{name}: no thumb");
            assert!(
                cells.iter().all(|&(x, _)| x == cells[0].0),
                "{name}: {cells:?}"
            );
            assert!(cells.iter().any(|&(_, y)| y == top), "{name}: {cells:?}");
            assert!(cells.iter().all(|&(_, y)| y < bottom), "{name}: {cells:?}");

            // At the last row the thumb reaches the bottom and leaves the top.
            if name == "dirs" {
                picker.source().selected = 39;
            } else {
                picker.browser().selected = 39;
            }
            let cells = thumb(picker);
            assert!(cells.iter().any(|&(_, y)| y == bottom), "{name}: {cells:?}");
            assert!(cells.iter().all(|&(_, y)| y > top), "{name}: {cells:?}");
        }

        // A list that fits has nothing hidden to point at.
        let mut dirs = test_picker(small.clone(), Mode::Dirs);
        scanned(&mut dirs);
        assert!(thumb(&mut dirs).is_empty());
        let mut browse = test_picker(small.clone(), Mode::Browse);
        assert!(thumb(&mut browse).is_empty());

        crate::testing::remove_tree(&root);
        crate::testing::remove_tree(&small);
    }

    #[test]
    fn a_real_scan_hands_back_the_whole_path_it_found() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-roundtrip-{}", std::process::id()));
        let folder = root.join("outer").join("日本語 folder");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("note.txt");
        std::fs::write(&file, "x").unwrap();

        // Entries carry the name relative to the scan root, so what a mode
        // hands the shell is rebuilt rather than stored. It has to come back
        // byte for byte, including a name outside ASCII.
        let config = Config::default();
        for (mode, expected) in [(Mode::Dirs, folder.clone()), (Mode::Files, folder.clone())] {
            let mut source = Source::start(mode, &root, &config, config.scan_limit);
            source.finish_scan();
            let query = if mode == Mode::Files {
                "note.txt"
            } else {
                "日本語 folder"
            };
            source.set_query(query, false);
            while source.matcher.tick(TICK_MS).running {}
            let entry = source.selected_entry().expect("one match");
            // Files hand back the folder that holds the file.
            assert_eq!(entry.output_path(mode), expected, "{mode:?}");
        }
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn browse_opens_on_the_searched_folder_with_the_highlight_in_place() {
        let root = crate::testing::temp_dir().join(format!("tadoru-round-{}", std::process::id()));
        let inside = root.join("crypto");
        let nested = inside.join("aes");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let file = nested.join("aes_core.c");
        std::fs::write(&file, "").unwrap();
        let pick = |p: &std::path::Path| Some(Entry::absolute(p.to_path_buf()));

        // A list of this folder says what to show; the other two do not.
        assert_eq!(
            browse_reveal(Mode::Dirs, pick(&inside)),
            Some(inside.clone())
        );
        assert_eq!(browse_reveal(Mode::Files, pick(&file)), Some(file.clone()));
        for mode in [Mode::Recent, Mode::Favorites] {
            assert_eq!(
                browse_reveal(mode, pick(&elsewhere)),
                None,
                "{mode:?} chose where browse opened"
            );
        }
        assert_eq!(browse_reveal(Mode::Dirs, None), None);

        // Shown in place: the folder searched stays on screen, with the row
        // that was highlighted still highlighted. Stepping into it instead
        // replaced the list the reader had just been reading.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        picker.toggle_browse();
        assert_eq!(picker.mode, Mode::Browse);
        assert_eq!(picker.browser().cwd, root);
        let shown = picker.browser().selected_path();
        assert!(
            shown.is_some_and(|p| p.starts_with(&root)),
            "nothing selected"
        );

        // A highlight further down opens the folder that holds it, not the
        // folder it is, so its neighbours are still in view.
        let mut browser = Browser::new(root.clone());
        browser.reveal(&nested);
        assert_eq!(browser.cwd, inside);
        assert_eq!(browser.selected_path(), Some(nested.clone()));
        browser.reveal(&file);
        assert_eq!(browser.cwd, nested);
        assert_eq!(browser.selected_path(), Some(file));

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn browse_follows_the_search_once_the_search_has_moved() {
        let root = crate::testing::temp_dir().join(format!("tadoru-anchor-{}", std::process::id()));
        let deep = root.join("one").join("two");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        let mut picker = test_picker(deep.clone(), Mode::Dirs);

        // Browse opens where the search is.
        picker.toggle_browse();
        assert_eq!(picker.browser().cwd, deep);

        // Flipping back and forth keeps the place, which is the point of
        // browse remembering anything at all.
        picker.browser().up();
        let stopped = picker.browser().cwd.clone();
        picker.toggle_browse();
        picker.toggle_browse();
        assert_eq!(picker.browser().cwd, stopped);

        // Once the search moves somewhere else, browse lines up with it
        // rather than coming back to a folder from earlier on. Every way of
        // moving the search has to do this, not just the one tried by hand.
        for (name, mut move_root) in [
            (
                "Left",
                Box::new(|p: &mut Picker| {
                    p.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
                }) as Box<dyn FnMut(&mut Picker)>,
            ),
            (
                "Alt-Left",
                Box::new(|p: &mut Picker| {
                    p.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
                }),
            ),
            // Alt-Left has just used up the way back, so Ctrl is checked going
            // forward here. Going back with Ctrl has a test of its own.
            (
                "Ctrl-Right",
                Box::new(|p: &mut Picker| {
                    p.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
                }),
            ),
            (
                "the back button",
                Box::new(|p: &mut Picker| p.navigate(Nav::Back)),
            ),
        ] {
            picker.toggle_browse();
            let before = picker.root.clone();
            move_root(&mut picker);
            let moved = picker.root.clone();
            assert_ne!(moved, before, "{name} did not move the search");
            picker.toggle_browse();
            assert_eq!(picker.browser().cwd, moved, "browse stayed behind: {name}");
        }

        crate::testing::remove_tree(&root);
    }

    #[cfg(windows)]
    #[test]
    fn left_at_the_top_of_a_drive_lists_the_drives() {
        let top = crate::testing::temp_dir()
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();
        let mut picker = test_picker(top.clone(), Mode::Browse);
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(picker.browser().at_drives());
        // The drive just left is selected, so Enter goes to it.
        assert_eq!(picker.browser().target(), top);
        assert_eq!(picker.selected_path(), Some(top.clone()));

        // A search started from here starts at the drive selected. Favorites
        // is used because it scans nothing.
        picker.switch_to(Mode::Favorites);
        assert_eq!(picker.root, top);

        // With nothing matching the filter nothing is selected, and neither
        // the actions nor pinning are handed an empty path.
        picker.switch_to(Mode::Browse);
        assert!(picker.browser().at_drives());
        picker.browser().set_filter("no such drive");
        assert_eq!(picker.selected_path(), None);
        picker.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert!(
            picker
                .notice
                .as_deref()
                .unwrap()
                .starts_with("Nothing selected")
        );
    }

    #[test]
    fn the_tree_opens_folders_in_place_and_shares_browse_s_place() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-treeview-{}", std::process::id()));
        let inner = root.join("top/a");
        std::fs::create_dir_all(inner.join("deep")).unwrap();
        std::fs::write(inner.join("x.txt"), "").unwrap();
        let top = root.join("top");
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl_space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);

        let mut picker = test_picker(top.clone(), Mode::Browse);
        picker.browser().navigate_to(&inner);
        picker.handle_key(ctrl_space);
        picker.handle_key(key(KeyCode::Char('v')));
        assert!(picker.in_tree());
        assert_eq!(picker.tree().root, inner);

        // It opens on the row selected in the columns. Down to x.txt: Enter
        // goes to the folder holding it.
        assert_eq!(picker.selected_path(), Some(inner.join("deep")));
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected_path(), Some(inner.join("x.txt")));
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            Action::Accept
        ));
        assert_eq!(picker.tree().target(), inner);

        // Left from a row goes to the root line; Left there moves the root up
        // and browse with it, keeping the old root open and selected.
        picker.handle_key(key(KeyCode::Left));
        assert_eq!(picker.tree().selected, 0);
        picker.handle_key(key(KeyCode::Left));
        assert_eq!(picker.tree().root, top);
        assert_eq!(picker.browser().cwd, top);
        assert_eq!(picker.selected_path(), Some(inner.clone()));
        assert!(picker.tree().selected_node().open);
        // The move is in browse's history.
        picker.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(picker.browser().cwd, inner);
        assert_eq!(picker.tree().root, inner);

        // Back to the columns: the row chosen in the tree is selected there.
        assert_eq!(picker.selected_path(), Some(inner.join("deep")));
        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(ctrl_space);
        picker.handle_key(key(KeyCode::Char('v')));
        assert!(!picker.in_tree());
        assert_eq!(picker.browser().selected_path(), Some(inner.join("x.txt")));

        // The tree is remembered: from a search, Tab comes back to it, and a
        // search started from the tree starts at its root.
        picker.handle_key(ctrl_space);
        picker.handle_key(key(KeyCode::Char('v')));
        picker.switch_to(Mode::Dirs);
        assert_eq!(picker.root, inner);
        picker.switch_to(Mode::Browse);
        assert!(picker.in_tree());
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn typing_takes_fzf_s_marks_for_exact_start_end_and_leaving_out() {
        let root = crate::testing::temp_dir().join(format!("tadoru-exact-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a_x_b_c.txt"), "").unwrap();
        std::fs::write(root.join("abc.txt"), "").unwrap();
        std::fs::write(root.join("abc.md"), "").unwrap();
        let matched = |query: &str| {
            let mut source = Source::start(Mode::Files, &root, &Config::default(), 0);
            source.finish_scan();
            source.set_query(query, false);
            while source.matcher.tick(TICK_MS).running {}
            let snapshot = source.matcher.snapshot();
            let mut names: Vec<String> = (0..snapshot.matched_item_count())
                .filter_map(|n| snapshot.get_matched_item(n))
                .map(|item| item.data.display.clone())
                .collect();
            names.sort();
            names
        };
        // Plain letters match in order with gaps; a leading ' wants them
        // side by side, and ! leaves out what matches.
        assert_eq!(matched("abc"), ["a_x_b_c.txt", "abc.md", "abc.txt"]);
        assert_eq!(matched("'abc"), ["abc.md", "abc.txt"]);
        assert_eq!(matched("!'abc"), ["a_x_b_c.txt"]);
        // After !, the letters are taken side by side even without the '.
        assert_eq!(matched("!abc"), ["a_x_b_c.txt"]);
        // ^ holds to the start of the path and $ to its end.
        assert_eq!(matched("^a_x"), ["a_x_b_c.txt"]);
        assert_eq!(matched(".md$"), ["abc.md"]);
        // Words apart must all match.
        assert_eq!(matched("'abc txt$"), ["abc.txt"]);
        // Upper and lower case are not told apart.
        assert_eq!(matched("'ABC.MD"), ["abc.md"]);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn going_back_in_history_during_a_tree_search_shows_the_new_folder() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-treehist-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a/inner")).unwrap();
        std::fs::write(root.join("a/gauge.rs"), "").unwrap();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut picker = test_picker(root.join("a"), Mode::Browse);
        picker.tree_view = true;
        // Move up, so that there is somewhere to go back to, then search.
        picker.tree().selected = 0;
        picker.handle_key(key(KeyCode::Left));
        assert_eq!(picker.tree().root, root);
        picker.handle_key(key(KeyCode::Char('g')));
        picker.tree_search.as_mut().unwrap().finish_scan();
        while picker
            .tree_search
            .as_mut()
            .unwrap()
            .matcher
            .tick(TICK_MS)
            .running
        {}
        picker.rebuild_found();
        assert!(picker.found.is_some());

        // Ctrl-Left: back to a. The next pass of the run loop ticks the
        // search before drawing, as run_tui does.
        picker.navigate(Nav::Back);
        picker.tick_tree_search();
        assert_eq!(picker.browser().cwd, root.join("a"));
        assert!(
            picker.found.is_none(),
            "the search of another folder is over"
        );
        assert!(picker.tree_query.is_empty());
        let labels: Vec<&str> = picker
            .tree()
            .nodes()
            .iter()
            .map(|n| n.label.as_str())
            .collect();
        assert!(labels.iter().any(|l| l.starts_with("inner")), "{labels:?}");
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn a_double_click_on_a_match_leaves_the_search_as_right_does() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-treeclick-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src/ui")).unwrap();
        std::fs::write(root.join("src/ui/gauge.rs"), "").unwrap();
        std::fs::write(root.join("src/other.rs"), "").unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.tree_view = true;
        for c in "src".chars() {
            picker.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        picker.tree_search.as_mut().unwrap().finish_scan();
        while picker
            .tree_search
            .as_mut()
            .unwrap()
            .matcher
            .tick(TICK_MS)
            .running
        {}
        picker.rebuild_found();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 14)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let (area, first, _) = picker.mouse_rows;
        let row = picker
            .found
            .as_ref()
            .unwrap()
            .nodes()
            .iter()
            .position(|n| n.path == root.join("src"))
            .unwrap();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + 3,
            row: area.y + (row - first) as u16,
            modifiers: KeyModifiers::NONE,
        };
        picker.handle_mouse(click);
        picker.handle_mouse(click);
        // Not a folder read into the list of matches, where what it holds
        // would pass for matches: the tree of folders, opened to src.
        assert!(picker.found.is_none(), "still in the search");
        assert!(picker.tree_query.is_empty());
        assert_eq!(picker.selected_path(), Some(root.join("src")));
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn ctrl_n_names_the_selected_folder_s_favorite_from_the_screen() {
        let root = crate::testing::temp_dir().join(format!("tadoru-name-{}", std::process::id()));
        std::fs::create_dir_all(root.join("work")).unwrap();
        let config = root.join("config");
        std::fs::create_dir_all(&config).unwrap();
        // The picker reads and writes favorites through the configuration
        // folder, pointed at this test's own for as long as the guard lives.
        let _config = crate::testing::config_dir(&config);
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.browser().reveal(&root.join("work"));
        assert_eq!(picker.browser().target(), root.join("work"));

        picker.handle_key(ctrl('n'));
        assert!(picker.naming.is_some());
        for c in "wo rk".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        picker.handle_key(key(KeyCode::Enter));
        assert!(picker.naming.is_none());
        let file = crate::favorites::path().unwrap();
        let named = crate::favorites::find(&file, "work")
            .unwrap()
            .expect("named");
        assert_eq!(named.path, root.join("work"));
        assert!(picker.pinned.contains(&root.join("work")));
        assert!(picker.notice.as_deref().unwrap().ends_with("is now :work"));

        // Opens on the name it has; Esc changes nothing, an empty name clears it.
        picker.handle_key(ctrl('n'));
        assert_eq!(picker.naming.as_ref().unwrap().1, "work");
        picker.handle_key(key(KeyCode::Esc));
        assert!(crate::favorites::find(&file, "work").unwrap().is_some());
        picker.handle_key(ctrl('n'));
        for _ in 0..4 {
            picker.handle_key(key(KeyCode::Backspace));
        }
        picker.handle_key(key(KeyCode::Enter));
        assert!(crate::favorites::find(&file, "work").unwrap().is_none());
        assert!(picker.pinned.contains(&root.join("work")), "still pinned");
        drop(_config);
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[cfg(windows)]
    #[test]
    fn a_search_asked_for_on_a_share_opens_in_browse_and_says_why() {
        // The share is never touched: the path's prefix refuses the scan.
        let share = Path::new(r"\\server\share");
        let (mode, notice) = opening_screen(Mode::Dirs, share);
        assert_eq!(mode, Mode::Browse);
        assert!(notice.unwrap().starts_with("Opened in browse: "));
        assert_eq!(opening_screen(Mode::Files, share).0, Mode::Browse);
        // Browse itself is not changed.
        assert_eq!(opening_screen(Mode::Browse, share), (Mode::Browse, None));
        // A local folder opens in the search asked for.
        let local = std::env::current_dir().unwrap();
        assert_eq!(opening_screen(Mode::Dirs, &local), (Mode::Dirs, None));
    }

    #[test]
    fn ctrl_x_in_the_menu_sets_what_follows_an_action_for_the_rest_of_the_run() {
        let root = crate::testing::temp_dir().join(format!("tadoru-then-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.after_action = crate::actions::Then::Quit;
        picker.handle_key(ctrl('p'));
        assert!(picker.menu.is_some());
        assert_eq!(
            picker.menu.as_ref().unwrap().after,
            crate::actions::Then::Quit
        );
        picker.handle_key(ctrl('x'));
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(picker.menu.is_none());
        assert_eq!(picker.after_action, crate::actions::Then::Stay);
        // The next menu opens on the switched setting.
        picker.handle_key(ctrl('p'));
        assert_eq!(
            picker.menu.as_ref().unwrap().after,
            crate::actions::Then::Stay
        );
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn a_double_click_on_a_place_goes_there_as_right_does() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-dblclick-{}", std::process::id()));
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let config = root.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let _config = crate::testing::config_dir(&config);
        let file = crate::favorites::path().unwrap();
        crate::favorites::pin(&file, &root.join("notes"), Some(true), Some("notes")).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let source = picker.source_for(Mode::Favorites);
        source.finish_scan();
        while source.matcher.tick(TICK_MS).running {}
        source.clamp_selection();
        // Where the rows were last drawn.
        picker.mouse_places = (Rect::new(10, 10, 50, 4), 0, 1);
        let click = |picker: &mut Picker| {
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 12,
                row: 10,
                modifiers: KeyModifiers::NONE,
            });
        };

        // One click selects and keeps the list open; the second, on the
        // same row, takes the search to the place and closes it.
        click(&mut picker);
        assert_eq!(picker.places, Some(Mode::Favorites));
        assert_eq!(picker.selected_path(), Some(root.join("notes")));
        click(&mut picker);
        assert_eq!(picker.places, None);
        assert_eq!(picker.mode, Mode::Dirs);
        assert_eq!(picker.root, root.join("notes"));
        drop(_config);
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn the_favorites_list_opens_over_the_screen_and_enter_or_right_take_a_place() {
        let root = crate::testing::temp_dir().join(format!("tadoru-places-{}", std::process::id()));
        std::fs::create_dir_all(root.join("work/inner")).unwrap();
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let config = root.join("config");
        std::fs::create_dir_all(&config).unwrap();
        let _config = crate::testing::config_dir(&config);
        let file = crate::favorites::path().unwrap();
        crate::favorites::pin(&file, &root.join("work"), Some(true), Some("work")).unwrap();
        crate::favorites::pin(&file, &root.join("notes"), Some(true), Some("notes")).unwrap();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let settle = |picker: &mut Picker| {
            let source = picker.source_for(Mode::Favorites);
            source.finish_scan();
            while source.matcher.tick(TICK_MS).running {}
            source.clamp_selection();
        };

        // Over a search, the list takes the typing, and Enter chooses the
        // place while the search under it keeps what it had.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.set_query("inn");
        picker.handle_key(ctrl('s'));
        assert_eq!(picker.places, Some(Mode::Favorites));
        for c in "not".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        settle(&mut picker);
        assert_eq!(picker.places_query, "not");
        assert_eq!(picker.query, "inn", "the search's own filter is untouched");
        assert_eq!(picker.selected_path(), Some(root.join("notes")));
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            Action::Accept
        ));

        // Esc clears what was typed first, then closes the list.
        picker.handle_key(key(KeyCode::Esc));
        assert_eq!(picker.places_query, "");
        assert!(picker.places.is_some());
        picker.handle_key(key(KeyCode::Esc));
        assert!(picker.places.is_none());
        assert_eq!(picker.mode, Mode::Dirs);

        // Right takes the search to the place: it starts there, with the
        // filter cleared, and Ctrl-Left brings both back.
        picker.handle_key(ctrl('s'));
        for c in "work".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        settle(&mut picker);
        picker.handle_key(key(KeyCode::Right));
        assert!(picker.places.is_none());
        assert_eq!(picker.mode, Mode::Dirs);
        assert_eq!(picker.root, root.join("work"));
        assert_eq!(picker.query, "");
        picker.handle_key(ctrl_left());
        assert_eq!(picker.root, root);
        assert_eq!(picker.query, "inn");

        // Over browse, Right shows the place in its folder, selected.
        picker.handle_key(key(KeyCode::Tab));
        assert_eq!(picker.mode, Mode::Browse);
        picker.handle_key(ctrl('s'));
        for c in "work".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        settle(&mut picker);
        picker.handle_key(key(KeyCode::Right));
        assert_eq!(picker.browser().cwd, root);
        assert_eq!(picker.browser().target(), root.join("work"));

        // The tab bar has no tab for the lists.
        assert_eq!(
            tabs(),
            [
                (Mode::Dirs, false),
                (Mode::Files, false),
                (Mode::Browse, false),
                (Mode::Browse, true)
            ]
        );
        drop(_config);
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    fn ctrl_left() -> KeyEvent {
        KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL)
    }

    #[test]
    fn a_search_after_the_tree_moves_up_covers_the_new_top() {
        let root = crate::testing::temp_dir().join(format!("tadoru-treeup-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a/inner")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("b/openssl.h"), "").unwrap();
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let settle = |picker: &mut Picker| {
            picker.tree_search.as_mut().unwrap().finish_scan();
            while picker
                .tree_search
                .as_mut()
                .unwrap()
                .matcher
                .tick(TICK_MS)
                .running
            {}
            picker.rebuild_found();
        };

        // A search in a, cleared, keeps its scan of a for the next query.
        let mut picker = test_picker(root.join("a"), Mode::Browse);
        picker.tree_view = true;
        picker.handle_key(key(KeyCode::Char('x')));
        settle(&mut picker);
        picker.handle_key(key(KeyCode::Esc));
        // Left on the top line moves the tree up to root.
        picker.tree().selected = 0;
        picker.handle_key(key(KeyCode::Left));
        assert_eq!(picker.tree().root, root);

        // The next search covers root, not the a it was scanned under.
        for c in "openssl".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        settle(&mut picker);
        assert_eq!(
            picker.selected_path(),
            Some(root.join("b").join("openssl.h"))
        );
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn typing_in_the_tree_searches_everything_under_it() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-treefind-{}", std::process::id()));
        let deep = root.join("src/ui/widgets");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(deep.join("gauge.rs"), "").unwrap();
        std::fs::write(root.join("docs/notes.md"), "").unwrap();
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let settle = |picker: &mut Picker| {
            picker.tree_search.as_mut().unwrap().finish_scan();
            while picker
                .tree_search
                .as_mut()
                .unwrap()
                .matcher
                .tick(TICK_MS)
                .running
            {}
            picker.rebuild_found();
        };

        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.tree_view = true;
        for c in "gauge".chars() {
            picker.handle_key(key(c));
        }
        settle(&mut picker);
        // The match is shown under its folders, which are dimmed, and is
        // selected; Enter goes to the folder holding it.
        let found = picker.found.as_ref().unwrap();
        let shown: Vec<(PathBuf, bool)> = found
            .nodes()
            .iter()
            .skip(1)
            .map(|node| (node.path.clone(), node.dim))
            .collect();
        assert_eq!(
            shown,
            [
                (root.join("src"), true),
                (root.join("src/ui"), true),
                (deep.clone(), true),
                (deep.join("gauge.rs"), false),
            ]
        );
        assert_eq!(picker.selected_path(), Some(deep.join("gauge.rs")));
        // The letters that matched are coloured in its name, as in the lists.
        assert_eq!(
            picker.found_marks.get(&deep.join("gauge.rs")),
            Some(&vec![0, 1, 2, 3, 4])
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(100, 14)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let cells = |y: u16| {
            (0..100u16)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<Vec<_>>()
        };
        let row = (0..14u16)
            .find(|&y| cells(y).concat().contains("gauge.rs"))
            .expect("the match is drawn");
        let line = cells(row);
        let at = (0..100 - 8)
            .find(|&x| line[x..x + 8].concat() == "gauge.rs")
            .unwrap() as u16;
        assert_eq!(buffer[(at, row)].fg, theme::MATCH.fg.unwrap());
        assert_ne!(buffer[(at + 5, row)].fg, theme::MATCH.fg.unwrap());
        assert!(matches!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Accept
        ));
        assert_eq!(picker.shown_tree().target(), deep);

        // Right leaves the search for the tree of folders, opened down to it.
        picker.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(picker.found.is_none());
        assert!(picker.tree_query.is_empty());
        assert_eq!(picker.selected_path(), Some(deep.join("gauge.rs")));
        assert!(
            picker
                .tree()
                .nodes()
                .iter()
                .any(|n| n.path == deep && n.open)
        );

        // Backspace deletes what was typed; Esc clears it and the tree is back.
        for c in "notes".chars() {
            picker.handle_key(key(c));
        }
        settle(&mut picker);
        assert_eq!(picker.selected_path(), Some(root.join("docs/notes.md")));
        picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(picker.tree_query, "note");
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(picker.tree_query.is_empty());
        assert!(picker.found.is_none());
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn ctrl_arrows_step_through_history_where_alt_arrows_are_taken() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-ctrlnav-{}", std::process::id()));
        let middle = root.join("a");
        let deep = middle.join("b");
        std::fs::create_dir_all(&deep).unwrap();
        let ctrl = |code| KeyEvent::new(code, KeyModifiers::CONTROL);

        // In browse: jump straight to a/b, so going back (to the root) and
        // going up (to a) lead to different places and cannot be confused.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.browser().navigate_to(&deep);
        assert_eq!(picker.browser().cwd, deep);
        picker.handle_key(ctrl(KeyCode::Left));
        assert_eq!(
            picker.browser().cwd,
            root,
            "Ctrl-Left climbed instead of going back"
        );
        picker.handle_key(ctrl(KeyCode::Right));
        assert_eq!(picker.browser().cwd, deep);
        // The plain arrow still climbs one level.
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(picker.browser().cwd, middle);

        // In a search, the same keys walk the places the search started from.
        let mut picker = test_picker(deep.clone(), Mode::Dirs);
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(picker.root, middle);
        picker.handle_key(ctrl(KeyCode::Left));
        assert_eq!(picker.root, deep);
        picker.handle_key(ctrl(KeyCode::Right));
        assert_eq!(picker.root, middle);
        // Ctrl-T goes back as Ctrl-Left does, as after a tag jump in Vim.
        picker.handle_key(ctrl(KeyCode::Char('t')));
        assert_eq!(picker.root, deep);
        drop(picker);

        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.browser().navigate_to(&deep);
        picker.handle_key(ctrl(KeyCode::Char('t')));
        assert_eq!(picker.browser().cwd, root);
        // AltGr with t types a character; it does not go back.
        picker.handle_key(ctrl(KeyCode::Right));
        picker.handle_key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(picker.browser().cwd, deep);

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn leaving_browse_somewhere_else_is_announced_and_can_be_undone() {
        let root = crate::testing::temp_dir().join(format!("tadoru-roots-{}", std::process::id()));
        let deep = root.join("one").join("two");
        std::fs::create_dir_all(&deep).unwrap();
        let mut picker = test_picker(deep.clone(), Mode::Dirs);
        picker.query = "keep".into();

        // Wander up in browse, then leave: the search now covers somewhere
        // the reader never chose, which is the whole complaint.
        picker.toggle_browse();
        assert_eq!(picker.mode, Mode::Browse);
        picker.browser().up();
        picker.browser().up();
        picker.toggle_browse();
        assert_eq!(picker.root, root);
        // Saying so is what stops it happening unnoticed.
        let notice = picker.notice.clone().expect("no notice");
        assert!(notice.contains("Ctrl-Left"), "{notice}");

        // One key puts it back, with what had been typed there.
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(picker.root, deep);
        assert_eq!(picker.query, "keep");
        // And forward returns to where it had gone.
        picker.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
        assert_eq!(picker.root, root);

        // Widening by hand is part of the same trail.
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(picker.root, root.parent().unwrap());
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(picker.root, root);

        // Nothing further back is said plainly rather than ignored.
        while picker.root_history(false) {}
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        let notice = picker.notice.clone().expect("no notice");
        assert!(notice.contains("No earlier"), "{notice}");

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn the_path_in_the_search_header_climbs_to_its_ancestors() {
        let root = crate::testing::temp_dir().join(format!("tadoru-widen-{}", std::process::id()));
        let deep = root.join("one/two");
        std::fs::create_dir_all(&deep).unwrap();
        let mut picker = test_picker(deep.clone(), Mode::Dirs);
        picker.query = "keep".into();

        // Left widens the search by one level, as it climbs one in browse.
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(picker.root, root.join("one"));
        // The typed filter survives, so widening does not restart the search.
        assert_eq!(picker.query, "keep");

        // A click on a step of the path goes straight there.
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let (area, target) = picker
            .mouse_crumbs
            .iter()
            .find(|(_, dir)| dir == &root)
            .expect("the root is a step of the path")
            .clone();
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(picker.root, target);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn alt_chars_do_not_reach_the_search_or_browse_filter() {
        let root = crate::testing::temp_dir().join(format!("tadoru-alt-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        for mode in [Mode::Dirs, Mode::Browse] {
            let mut picker = test_picker(root.clone(), mode);
            picker.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT));
            let filter = |p: &mut Picker| match mode {
                Mode::Browse => p.browser().filter.clone(),
                _ => p.query.clone(),
            };
            assert!(filter(&mut picker).is_empty(), "{mode:?}");
            picker.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
            assert_eq!(filter(&mut picker), "d", "{mode:?}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_click_left_over_from_the_action_menu_does_not_reach_the_list() {
        let root = crate::testing::temp_dir().join(format!("tadoru-guard-{}", std::process::id()));
        let child = root.join("child");
        std::fs::create_dir_all(child.join("grandchild")).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.browser().move_selection(0);
        let start = picker.browser().cwd.clone();
        picker.mouse_rows = (Rect::new(0, 0, 20, 5), 0, 1);
        let click = |x, y| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };

        // Picking from the menu closes it under the pointer, so the rest of a
        // quick double tap must not land on whatever moved into that spot.
        picker.block_clicks();
        picker.handle_mouse(click(0, 0));
        assert_eq!(picker.browser().cwd, start, "a leftover click got through");

        // The wheel is not a press and keeps working.
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            ..click(0, 0)
        });

        // Once the moment has passed, clicks count again.
        picker.clicks_blocked_until = Some(std::time::Instant::now());
        assert!(!picker.clicks_blocked());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn opening_a_path_elsewhere_always_reports_what_happened() {
        let root = crate::testing::temp_dir().join(format!("tadoru-report-{}", std::process::id()));
        let file = root.join("report.xlsx");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, "").unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);

        // Launching an application changes nothing on this screen, so both
        // outcomes have to be said out loud, by name rather than by full path.
        picker.report_open(&file, Ok(()));
        assert_eq!(picker.notice.as_deref(), Some("Opened: report.xlsx"));

        // A confirmation is about something already finished, so it retires
        // itself rather than sitting on the screen.
        assert!(picker.notice_until.is_some());
        picker.expire_notice();
        assert!(picker.notice.is_some(), "it should not vanish immediately");
        picker.notice_until = Some(std::time::Instant::now());
        picker.expire_notice();
        assert!(
            picker.notice.is_none(),
            "it should retire once its time is up"
        );

        // A failure has to be read, so it waits for the next action instead.
        picker.report_open(
            &file,
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
        );
        let notice = picker.notice.clone().unwrap();
        assert!(notice.starts_with("Cannot open report.xlsx"), "{notice}");
        assert!(notice.contains("missing"), "{notice}");
        assert!(picker.notice_until.is_none(), "a failure must not time out");
        picker.expire_notice();
        assert!(picker.notice.is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_mode_tabs_answer_clicks_from_the_border_they_moved_to() {
        use ratatui::backend::TestBackend;
        let root = crate::testing::temp_dir().join(format!("tadoru-tabs-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let area = Rect::new(0, 0, 100, 12);
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| picker.render(area, frame)).unwrap();

        // The tabs sit on the top border, one row above the box contents.
        assert_eq!(picker.mouse_header.y, area.y);
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.mouse_modes_x,
            row: picker.mouse_header.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(picker.mode, Mode::Dirs);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_lists_of_places_open_from_buttons_after_the_tabs() {
        use ratatui::backend::TestBackend;
        let root =
            crate::testing::temp_dir().join(format!("tadoru-buttons-{}", std::process::id()));
        std::fs::create_dir_all(root.join("config")).unwrap();
        let _config = crate::testing::config_dir(&root.join("config"));
        let top_row = |picker: &mut Picker, width: u16| {
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
                .draw(|frame| picker.render(frame.area(), frame))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..width)
                .map(|x| buffer[(x, 0)].symbol().to_string())
                .collect::<Vec<_>>()
        };
        let find = |cells: &[String], text: &str| -> Option<u16> {
            let len = text.chars().count();
            (0..=cells.len() - len)
                .find(|&x| cells[x..x + len].concat() == text)
                .map(|x| x as u16)
        };
        let click = |picker: &mut Picker, column: u16| {
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
        };

        // Outside the brackets, each with its icon.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        let row = top_row(&mut picker, 100);
        let favorites = find(&row, "[browse|tree]  ★ favorites  ◷ recent ").unwrap() + 15;
        let recent = find(&row, "◷ recent").unwrap();

        // A click opens the list, the other button swaps, and the same
        // button closes it, as the keys do. The screen under it stays.
        click(&mut picker, favorites);
        assert_eq!(picker.places, Some(Mode::Favorites));
        assert_eq!(picker.mode, Mode::Dirs);
        click(&mut picker, recent + 5);
        assert_eq!(picker.places, Some(Mode::Recent));
        click(&mut picker, recent);
        assert_eq!(picker.places, None);

        // A screen too narrow for the buttons keeps the tabs and drops them,
        // and a click where they would have been does nothing.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let row = top_row(&mut picker, 40);
        assert!(find(&row, "[dirs|files]  [browse|tree]").is_some());
        assert!(find(&row, "favorites").is_none(), "{}", row.concat());
        click(&mut picker, 33);
        assert_eq!(picker.places, None);
        drop(_config);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn browse_sits_apart_from_the_searches_and_each_tab_takes_its_own_clicks() {
        use ratatui::backend::TestBackend;
        let root = crate::testing::temp_dir().join(format!("tadoru-groups-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // The top border, one cell per column: the border glyphs are multi-byte.
        let top_row = |picker: &mut Picker| {
            let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
            terminal
                .draw(|frame| picker.render(frame.area(), frame))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..100u16)
                .map(|x| buffer[(x, 0)].symbol().to_string())
                .collect::<Vec<_>>()
        };
        let find = |cells: &[String], text: &str| -> u16 {
            let len = text.chars().count();
            (0..=cells.len() - len)
                .find(|&x| cells[x..x + len].concat() == text)
                .unwrap_or_else(|| panic!("{text:?} not in {}", cells.concat())) as u16
        };
        let click = |picker: &mut Picker, column: u16| {
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
        };

        // The two searches share one bracket; browse, which Tab pairs with
        // them, has its own, with the columns and the tree side by side.
        // Recent and favorites are lists over the screen, not tabs.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        find(&top_row(&mut picker), "[dirs|files]  [browse|tree]");

        // The first and last letter of each label switch to that mode.
        for mode in [Mode::Dirs, Mode::Files, Mode::Browse] {
            let start_in = if mode == Mode::Browse {
                Mode::Dirs
            } else {
                Mode::Browse
            };
            for offset in [0, mode.label().len() as u16 - 1] {
                let mut picker = test_picker(root.clone(), start_in);
                let at = find(&top_row(&mut picker), mode.label()) + offset;
                click(&mut picker, at);
                assert_eq!(picker.mode, mode, "{} at +{offset}", mode.label());
            }
        }

        // The space between the groups switches nothing.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let gap = find(&top_row(&mut picker), "]  [") + 1;
        click(&mut picker, gap);
        assert_eq!(picker.mode, Mode::Browse, "the gap switched modes");

        // tree draws browse as a tree, from browse or from a search, and
        // browse draws it as columns again.
        for start_in in [Mode::Browse, Mode::Dirs] {
            let mut picker = test_picker(root.clone(), start_in);
            let at = find(&top_row(&mut picker), "|tree") + 1;
            click(&mut picker, at);
            assert!(picker.in_tree(), "tree from {start_in:?}");
            let at = find(&top_row(&mut picker), "browse");
            click(&mut picker, at);
            assert_eq!(picker.mode, Mode::Browse);
            assert!(!picker.tree_view, "browse from the tree");
        }

        // From a search, browse is columns even if the tree was used last.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let at = find(&top_row(&mut picker), "|tree") + 1;
        click(&mut picker, at);
        let at = find(&top_row(&mut picker), "dirs");
        click(&mut picker, at);
        assert_eq!(picker.mode, Mode::Dirs);
        let at = find(&top_row(&mut picker), "browse");
        click(&mut picker, at);
        assert!(picker.mode == Mode::Browse && !picker.tree_view);

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn clicking_a_step_of_the_header_path_jumps_to_that_ancestor() {
        use ratatui::backend::TestBackend;
        let root = crate::testing::temp_dir().join(format!("tadoru-crumb-{}", std::process::id()));
        let deep = root.join("one").join("two").join("three");
        std::fs::create_dir_all(&deep).unwrap();
        let mut picker = test_picker(deep.clone(), Mode::Browse);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| picker.render(Rect::new(0, 0, 120, 20), frame))
            .unwrap();

        // Every visible step offers the directory it names, the last being
        // where we already are.
        let target = root.join("one");
        let (area, _) = picker
            .mouse_crumbs
            .iter()
            .find(|(_, dir)| dir == &target)
            .expect("the ancestor is clickable")
            .clone();
        picker.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(picker.browser().cwd, target);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn header_buttons_move_back_forward_and_up_and_dim_where_they_cannot() {
        use ratatui::backend::TestBackend;
        let root = crate::testing::temp_dir().join(format!("tadoru-nav-{}", std::process::id()));
        let child = root.join("child");
        std::fs::create_dir_all(child.join("grandchild")).unwrap();
        let mut picker = test_picker(child.clone(), Mode::Browse);
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let draw = |picker: &mut Picker, terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| picker.render(Rect::new(0, 0, 90, 20), frame))
                .unwrap();
        };
        let button = |picker: &Picker, nav: Nav| {
            picker
                .mouse_nav
                .iter()
                .find(|(_, kind)| *kind == nav)
                .map(|(area, _)| *area)
        };
        let click = |picker: &mut Picker, area: Rect| {
            picker.handle_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            });
        };

        // Nothing visited yet, so only the step to the parent is offered.
        draw(&mut picker, &mut terminal);
        assert!(button(&picker, Nav::Back).is_none());
        assert!(button(&picker, Nav::Forward).is_none());
        let up = button(&picker, Nav::Up).expect("up is available below the root");
        click(&mut picker, up);
        assert_eq!(picker.browser().cwd, root);

        // Having moved, back becomes available and returns to the child.
        draw(&mut picker, &mut terminal);
        let back = button(&picker, Nav::Back).expect("back after moving up");
        click(&mut picker, back);
        assert_eq!(picker.browser().cwd, child);

        // And forward retraces that step.
        draw(&mut picker, &mut terminal);
        let forward = button(&picker, Nav::Forward).expect("forward after going back");
        click(&mut picker, forward);
        assert_eq!(picker.browser().cwd, root);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn drawing_never_reads_a_directory_so_a_scroll_burst_costs_nothing() {
        use ratatui::backend::TestBackend;
        let root =
            crate::testing::temp_dir().join(format!("tadoru-preview-idle-{}", std::process::id()));
        std::fs::create_dir_all(root.join("child")).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();

        // Every frame drawn while input is still queued must leave the listing
        // alone, otherwise a fast scroll queues one directory read per notch.
        for _ in 0..5 {
            terminal
                .draw(|frame| picker.render(Rect::new(0, 0, 90, 20), frame))
                .unwrap();
            assert!(
                picker.preview.is_none(),
                "rendering loaded a directory listing"
            );
        }

        // Once the queue drains the update phase fills it in.
        picker.refresh_preview();
        let (path, names) = picker.preview.clone().expect("listing after refresh");
        assert_eq!(path, root.join("child"));
        assert!(names.is_empty(), "child is empty: {names:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn search_preview_click_targets_child_without_changing_search_for_menu() {
        use ratatui::backend::TestBackend;
        let root =
            std::env::temp_dir().join(format!("tadoru-preview-mouse-{}", std::process::id()));
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        // The real loop loads the listing before drawing; rendering never does.
        picker.preview = Some((root.clone(), list_dir(&root)));
        terminal
            .draw(|frame| picker.render_preview(Rect::new(40, 3, 35, 12), frame, Some(&root)))
            .unwrap();
        let (area, target) = picker.mouse_paths.first().unwrap().clone();
        assert_eq!(target, child);
        let mut click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::CONTROL,
        };
        picker.handle_mouse(click);
        assert_eq!(picker.menu.as_ref().unwrap().target, child);
        assert_eq!(picker.mode, Mode::Dirs);
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        click.kind = MouseEventKind::Down(MouseButton::Left);
        click.modifiers = KeyModifiers::NONE;
        picker.handle_mouse(click);
        assert_eq!(picker.mode, Mode::Browse);
        assert_eq!(picker.browser().cwd, child);
        drop(picker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ctrl_space_opens_the_keys_and_a_plain_letter_runs_one() {
        let root = crate::testing::temp_dir().join(format!("tadoru-keys-{}", std::process::id()));
        let inner = root.join("only/inner");
        std::fs::create_dir_all(&inner).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.set_query("inner");
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        let ctrl_space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL);
        let plain = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);

        // The letter goes to the panel, not into the filter, and the panel
        // closes once it has run something.
        picker.handle_key(ctrl_space);
        assert!(picker.keys.is_some());
        picker.handle_key(plain('s'));
        assert_eq!(picker.places, Some(Mode::Favorites));
        assert!(picker.keys.is_none());
        assert_eq!(picker.query, "inner");
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(picker.places.is_none());

        // Esc closes the panel and nothing else: the filter is still there,
        // where Esc on the picker itself would have cleared it.
        picker.handle_key(ctrl_space);
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(picker.keys.is_none());
        assert_eq!(picker.query, "inner");
        assert_eq!(picker.mode, Mode::Dirs);

        // A row that stands for a shortcut does what the shortcut does.
        picker.handle_key(ctrl_space);
        picker.handle_key(plain('d'));
        while picker.source().matcher.tick(TICK_MS).running {}
        picker.handle_key(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE));
        assert!(picker.keys.is_some(), "a bare NUL is Ctrl-Space too");
        picker.handle_key(plain('l'));
        assert_eq!(picker.mode, Mode::Browse);
        assert_eq!(picker.browser().cwd, inner);
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn ctrl_with_a_letter_goes_straight_to_that_search() {
        let root = crate::testing::temp_dir().join(format!("tadoru-jump-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);

        // The lists open over the screen, which stays as it was; the
        // searches replace it, and close a list that was open.
        picker.handle_key(ctrl('s'));
        assert_eq!(picker.places, Some(Mode::Favorites));
        assert_eq!(picker.mode, Mode::Dirs);
        picker.handle_key(ctrl('r'));
        assert_eq!(picker.places, Some(Mode::Recent));
        picker.handle_key(ctrl('r'));
        assert!(picker.places.is_none(), "the same key closes the list");
        picker.handle_key(ctrl('s'));
        picker.handle_key(ctrl('f'));
        assert!(picker.places.is_none());
        assert_eq!(picker.mode, Mode::Files);
        picker.handle_key(ctrl('d'));
        assert_eq!(picker.mode, Mode::Dirs);
        // Over browse too, and Tab from there still goes to the search.
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(picker.mode, Mode::Browse);
        picker.handle_key(ctrl('s'));
        assert_eq!(picker.places, Some(Mode::Favorites));
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert!(picker.places.is_none());
        assert_eq!(picker.mode, Mode::Dirs);

        // The plain letter and AltGr leave the mode alone, and the plain
        // letter is typed into the filter.
        picker.handle_key(ctrl('f'));
        for key in [
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE),
            KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
        ] {
            picker.handle_key(key);
            assert_eq!(picker.mode, Mode::Files, "{key:?}");
        }
        assert_eq!(picker.query, "s");
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn backspace_goes_up_only_once_it_has_not_been_clearing_for_a_while() {
        let root = crate::testing::temp_dir().join(format!("tadoru-ctrl-h-{}", std::process::id()));
        let inner = root.join("only/inner");
        std::fs::create_dir_all(&inner).unwrap();
        let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let long_ago = Some(std::time::Instant::now() - ERASE_PAUSE - Duration::from_millis(1));

        // In browse: deleting, then a press too many, stays put; after the
        // pause the same press goes up, and so does the next one at once.
        let mut picker = test_picker(inner.clone(), Mode::Browse);
        picker.browser().set_filter("x");
        picker.handle_key(backspace);
        assert_eq!(picker.browser().filter, "");
        picker.handle_key(backspace);
        picker.handle_key(backspace);
        assert_eq!(
            picker.browser().cwd,
            inner,
            "a press too many must not move"
        );
        picker.last_erase = long_ago;
        picker.handle_key(backspace);
        assert_eq!(picker.browser().cwd, root.join("only"));
        picker.handle_key(backspace);
        assert_eq!(
            picker.browser().cwd,
            root,
            "going up has no pause of its own"
        );
        drop(picker);

        // Esc clears without the pause: Backspace right after it goes up.
        let mut picker = test_picker(inner.clone(), Mode::Browse);
        picker.browser().set_filter("x");
        picker.handle_key(backspace);
        picker.handle_key(esc);
        picker.handle_key(backspace);
        assert_eq!(picker.browser().cwd, root.join("only"));
        drop(picker);

        // Ctrl-H is Left and never waits: it goes up even mid-clearing.
        let mut picker = test_picker(inner.clone(), Mode::Browse);
        picker.browser().set_filter("x");
        picker.handle_key(backspace);
        picker.handle_key(ctrl_h);
        assert_eq!(picker.browser().cwd, root.join("only"));
        drop(picker);

        // In the search the same, widening by a level and keeping nothing.
        let mut picker = test_picker(inner.clone(), Mode::Dirs);
        picker.set_query("ab");
        picker.handle_key(backspace);
        picker.handle_key(backspace);
        picker.handle_key(backspace);
        assert_eq!(picker.query, "");
        assert_eq!(picker.root, inner, "a press too many must not widen");
        picker.last_erase = long_ago;
        picker.handle_key(backspace);
        assert_eq!(picker.root, root.join("only"));
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn right_goes_into_the_selection_and_tab_searches_from_there() {
        let root = crate::testing::temp_dir().join(format!("tadoru-right-{}", std::process::id()));
        let inner = root.join("only/inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(root.join("only/note.txt"), "").unwrap();
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);

        // A folder is stepped into. Enter would have ended the session there.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.set_query("inner");
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        assert!(matches!(picker.handle_key(right), Action::Continue));
        assert_eq!(picker.mode, Mode::Browse);
        assert_eq!(picker.browser().cwd, inner);
        // Tab goes back to the search, which now covers the folder reached.
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(picker.mode, Mode::Dirs);
        assert_eq!(picker.root, inner);
        drop(picker);

        // The lists of places open over the screen, and Right in them takes
        // the screen to the place chosen: a search goes on from there.
        let mut picker = test_picker(root.clone(), Mode::Files);
        picker.switch_to(Mode::Favorites);
        assert_eq!(picker.places, Some(Mode::Favorites));
        picker.query = "inn".into();
        picker.go_to_place(&inner);
        assert!(picker.places.is_none());
        assert_eq!(picker.mode, Mode::Files);
        assert_eq!(picker.root, inner);
        // The filter that found the favorite is not carried into it, and going
        // back restores it along with where the search started.
        assert_eq!(picker.query, "");
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(picker.root, root);
        assert_eq!(picker.query, "inn");
        drop(picker);

        // Over browse, the place is shown in its folder and selected, so
        // Enter goes to it rather than to whatever sorts first inside it.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.switch_to(Mode::Favorites);
        picker.go_to_place(&inner);
        assert_eq!(picker.mode, Mode::Browse);
        assert_eq!(picker.browser().cwd, root.join("only"));
        assert_eq!(picker.browser().target(), inner);
        // Right again goes into it.
        picker.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(picker.browser().cwd, inner);
        drop(picker);

        // A file opens the folder holding it, with the file selected. Ctrl-L
        // stands in for Right here as it does in browse.
        let mut picker = test_picker(root.clone(), Mode::Files);
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        picker.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(picker.mode, Mode::Browse);
        assert_eq!(picker.browser().cwd, root.join("only"));
        assert_eq!(
            picker.browser().selected_path(),
            Some(root.join("only/note.txt"))
        );
        drop(picker);

        // With nothing selected the key does nothing.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.set_query("no such folder");
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        picker.handle_key(right);
        assert_eq!(picker.mode, Mode::Dirs);
        drop(picker);

        // A folder deleted since it was listed, as a favorite can be, is
        // reported and the search stays where it was.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.set_query("inner");
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        std::fs::remove_dir(&inner).unwrap();
        picker.handle_key(right);
        assert_eq!(picker.mode, Mode::Dirs);
        assert!(picker.notice.as_deref().unwrap().contains("inner"));
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn mouse_uses_rendered_rows_and_ignores_blank_space_and_disabled_input() {
        use ratatui::backend::TestBackend;
        let root = std::env::temp_dir().join(format!("tadoru-mouse-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for index in 0..30 {
            std::fs::create_dir_all(root.join(format!("item-{index:02}"))).unwrap();
        }
        let child = root.join("item-00/child");
        std::fs::create_dir_all(&child).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.browser().selected = 20;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| picker.render(Rect::new(2, 5, 90, 15), frame))
            .unwrap();
        let (area, first, _) = picker.mouse_rows;
        assert!(first > 0);
        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        picker.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y,
        ));
        assert_eq!(picker.browser().selected, first);
        picker.handle_mouse(mouse(MouseEventKind::ScrollDown, area.x, area.y));
        assert_eq!(picker.browser().selected, first + 3);
        picker.config.mouse = false;
        picker.handle_mouse(mouse(MouseEventKind::ScrollUp, area.x, area.y));
        assert_eq!(picker.browser().selected, first + 3);
        picker.config.mouse = true;
        picker.set_query("item-00");
        picker.refresh_preview();
        terminal
            .draw(|frame| picker.render(Rect::new(2, 5, 90, 15), frame))
            .unwrap();
        let (area, _, _) = picker.mouse_rows;
        picker.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            area.x,
            area.y + 2,
        ));
        assert_eq!(picker.browser().selected, 0);
        let (hit, _) = picker
            .mouse_paths
            .iter()
            .find(|(_, path)| path == &child)
            .unwrap()
            .clone();
        picker.handle_mouse(MouseEvent {
            modifiers: KeyModifiers::CONTROL,
            ..mouse(MouseEventKind::Down(MouseButton::Right), hit.x, hit.y)
        });
        assert_eq!(picker.menu.as_ref().unwrap().target, child);
        assert_eq!(picker.browser().cwd, root);
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(picker.menu.is_none());
        picker.handle_mouse(MouseEvent {
            modifiers: KeyModifiers::CONTROL,
            ..mouse(MouseEventKind::Down(MouseButton::Right), area.x, area.y)
        });
        assert_eq!(picker.menu.as_ref().unwrap().target, root.join("item-00"));
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.x, hit.y));
        assert_eq!(picker.browser().cwd, child);
        terminal
            .draw(|frame| picker.render(Rect::new(2, 5, 90, 15), frame))
            .unwrap();
        // Another sibling in the parent column navigates to it.
        let sibling = root.join("item-00/sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        picker.browser().refresh();
        terminal
            .draw(|frame| picker.render(Rect::new(2, 5, 90, 15), frame))
            .unwrap();
        let (hit, _) = picker
            .mouse_paths
            .iter()
            .find(|(_, path)| path == &sibling)
            .unwrap()
            .clone();
        picker.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), hit.x, hit.y));
        assert_eq!(picker.browser().cwd, sibling);
        // The tabs do not start at the first column: the navigation
        // buttons sit in front of them in browse mode.
        let header = picker.mouse_header;
        picker.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            picker.mouse_modes_x,
            header.y,
        ));
        assert_eq!(picker.mode, Mode::Dirs);
        drop(picker);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn network_scan_finishes_with_an_error_and_no_candidates() {
        for mode in [Mode::Dirs, Mode::Files] {
            let mut source = Source::start(
                mode,
                Path::new(r"\\unreachable.invalid\share"),
                &Config::default(),
                0,
            );
            source.finish_scan();
            assert!(source.scan_error.as_ref().unwrap().contains("blocked"));
            assert!(source.scan_done.load(Ordering::Acquire));
            assert_eq!(source.matcher.snapshot().item_count(), 0);
        }
    }

    #[test]
    #[cfg(windows)]
    fn the_tree_says_why_a_network_folder_was_not_searched() {
        use ratatui::backend::TestBackend;
        let root =
            crate::testing::temp_dir().join(format!("tadoru-treenet-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut picker = test_picker(root.clone(), Mode::Browse);
        picker.tree_view = true;
        picker.tree();
        // A search already started on a share, which is refused before the
        // server is contacted.
        let mut source = Source::start(
            Mode::Browse,
            Path::new(r"\\unreachable.invalid\share"),
            &Config::default(),
            0,
        );
        source.finish_scan();
        picker.tree_search = Some(source);
        picker.tree_query = "notes".into();
        picker.tick_tree_search();
        let mut terminal = Terminal::new(TestBackend::new(160, 12)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let screen: String = (0..12u16)
            .flat_map(|y| (0..160u16).map(move |x| (x, y)))
            .map(|(x, y)| buffer[(x, y)].symbol().to_string())
            .collect();
        assert!(
            screen.contains("Not searched: Recursive scan blocked"),
            "{screen}"
        );
        drop(picker);
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn escape_clears_active_filter_before_exit_and_ctrl_c_exits_immediately() {
        for mode in [Mode::Browse, Mode::Dirs] {
            let root = std::env::temp_dir().join("tadoru-escape-nonexistent-root");
            let mut picker = test_picker(root, mode);
            picker.set_query("ss");
            let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
            assert!(matches!(picker.handle_key(escape), Action::Continue));
            assert!(if mode == Mode::Browse {
                picker.browser().filter.is_empty()
            } else {
                picker.query.is_empty()
            });
            assert!(matches!(picker.handle_key(escape), Action::Cancel));
            picker.set_query("ss");
            assert!(matches!(
                picker.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
                Action::Cancel
            ));
        }
    }

    #[test]
    fn history_errors_are_visible_on_entry_and_mode_switch_but_empty_history_is_distinct() {
        use ratatui::backend::TestBackend;
        for via_tab in [false, true] {
            for error in [
                Some("cannot run zoxide: missing"),
                Some("zoxide query failed: fixture error"),
                None,
            ] {
                let root = std::env::temp_dir().join("tadoru-nonexistent-history-test-root");
                let mut picker = test_picker(root.clone(), Mode::Browse);
                let mut source = Source::start(
                    Mode::Dirs,
                    &root,
                    &Config::default(),
                    Config::default().scan_limit,
                );
                source.finish_scan();
                source.mode = Mode::Recent;
                source.scanner = Some(std::thread::spawn(move || {
                    error.map_or(Ok(Vec::new()), |error| Err(error.to_string()))
                }));
                source.finish_scan();
                picker.sources[mode_index(Mode::Recent)] = Some(source);
                if via_tab {
                    picker.mode = Mode::Files;
                }
                picker.places = Some(Mode::Recent);
                let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
                terminal
                    .draw(|frame| picker.render(frame.area(), frame))
                    .unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                if let Some(error) = error {
                    assert!(text.contains(error), "{text}");
                    assert!(text.contains("F5: retry"));
                    assert!(!text.contains("No history yet"));
                } else {
                    assert!(text.contains("No history yet"));
                }
                // Esc closes the list and leaves the screen under it alone.
                picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert!(picker.places.is_none());
                assert_eq!(
                    picker.mode,
                    if via_tab { Mode::Files } else { Mode::Browse }
                );
            }
        }
    }

    /// What browsing costs, as a mode switch and a walk would do it.
    #[test]
    #[ignore = "local memory measurement; set TADORU_BENCH_ROOT and run in release mode"]
    fn benchmark_browse_memory() {
        let root = std::env::var_os("TADORU_BENCH_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let mib = || -> f64 {
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!("(Get-Process -Id {}).WorkingSet64", std::process::id()),
                ])
                .output()
                .expect("powershell");
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0)
                / 1048576.0
        };
        eprintln!("baseline mib={:.1}", mib());

        // Open the way c does, let the scan finish, then Tab across to browse.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        eprintln!("dirs_scanned mib={:.1}", mib());

        picker.toggle_browse();
        assert_eq!(picker.mode, Mode::Browse);
        eprintln!("entered_browse mib={:.1}", mib());

        // Walk down into the first directory at each level, then back up.
        for step in 1..=6 {
            let before = picker.browser().cwd.clone();
            picker.browser().enter();
            let cwd = picker.browser().cwd.clone();
            eprintln!("descend_{step} mib={:.1} cwd={}", mib(), cwd.display());
            if cwd == before {
                break;
            }
        }
        for _ in 0..6 {
            picker.browser().up();
        }
        eprintln!(
            "back_up mib={:.1} cwd={}",
            mib(),
            picker.browser().cwd.display()
        );

        // Tab out: the browse location becomes the scan root, so every list
        // starts again from there.
        picker.toggle_browse();
        picker.source().finish_scan();
        while picker.source().matcher.tick(TICK_MS).running {}
        let items = picker.source().matcher.snapshot().item_count();
        eprintln!(
            "left_browse_and_rescanned mode={} items={items} root={} mib={:.1}",
            picker.mode.label(),
            picker.root.display(),
            mib()
        );
    }

    /// What a scanned list costs to keep. Every mode visited keeps its own,
    /// so the total is what the reader sees in the task manager.
    #[test]
    #[ignore = "local memory measurement; set TADORU_BENCH_ROOT and run in release mode"]
    fn benchmark_memory() {
        let root = std::env::var_os("TADORU_BENCH_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let config = Config::default();
        let working_set = || -> f64 {
            let output = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!("(Get-Process -Id {}).WorkingSet64", std::process::id()),
                ])
                .output()
                .expect("powershell");
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0)
                / 1024.0
                / 1024.0
        };
        eprintln!("baseline_mib={:.1}", working_set());

        let ready = |mode: Mode| {
            // Sample while the walk runs, so the peak shows as well as the
            // resting size. The sort keys the walk builds live only until the
            // order is handed over, and would not show in a later reading.
            let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let sampler = {
                let stop = stop.clone();
                let peak = peak.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let output = std::process::Command::new("powershell")
                            .args([
                                "-NoProfile",
                                "-Command",
                                &format!("(Get-Process -Id {}).WorkingSet64", std::process::id()),
                            ])
                            .output()
                            .expect("powershell");
                        let now: u64 = String::from_utf8_lossy(&output.stdout)
                            .trim()
                            .parse()
                            .unwrap_or(0);
                        peak.fetch_max(now, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(15));
                    }
                })
            };
            let mut source = Source::start(mode, &root, &config, config.scan_limit);
            source.finish_scan();
            while source.matcher.tick(TICK_MS).running {}
            source.refresh_browse_order();
            let count = source.matcher.snapshot().item_count();
            stop.store(true, Ordering::Relaxed);
            let _ = sampler.join();
            eprintln!(
                "after_{}_items={count} mib={:.1} peak_mib={:.1}",
                mode.label(),
                working_set(),
                peak.load(Ordering::Relaxed) as f64 / 1024.0 / 1024.0
            );
            source
        };
        let dirs = ready(Mode::Dirs);
        let files = ready(Mode::Files);
        eprintln!("both_alive_mib={:.1}", working_set());

        // What the paths themselves weigh, against what the process holds.
        let snapshot = files.matcher.snapshot();
        let mut display = 0usize;
        let mut path = 0usize;
        let mut column = 0usize;
        let mut ascii = 0usize;
        for i in 0..snapshot.item_count() {
            let Some(item) = snapshot.get_item(i) else {
                continue;
            };
            display += item.data.display.len();
            if item.data.exact_path_kept() {
                path += 1;
            }
            let text = item.matcher_columns[0].slice(..);
            column += match text {
                nucleo::Utf32Str::Ascii(bytes) => {
                    ascii += 1;
                    bytes.len()
                }
                nucleo::Utf32Str::Unicode(chars) => chars.len() * 4,
            };
        }
        let mib = |bytes: usize| bytes as f64 / 1024.0 / 1024.0;
        eprintln!(
            "files display={:.1} match_column={:.1} MiB, ascii_rows={ascii} exact_paths_kept={path}",
            mib(display),
            mib(column)
        );
        drop(dirs);
        drop(files);
        eprintln!("after_drop_mib={:.1}", working_set());
    }

    /// Mode switching used to stall: leaving browse threw away every cached
    /// source on the drawing thread, and finishing a scan sorted its result
    /// there too. Both are measured here so a regression is visible.
    #[test]
    #[ignore = "local performance measurement; set TADORU_BENCH_ROOT and run in release mode"]
    fn benchmark_mode_switch() {
        use std::time::Instant;
        let root = std::env::var_os("TADORU_BENCH_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        let config = Config::default();

        let mut source = Source::start(Mode::Files, &root, &config, config.scan_limit);
        source.finish_scan();
        while source.matcher.tick(TICK_MS).running {}
        let count = source.matcher.snapshot().item_count();
        let start = Instant::now();
        source.refresh_browse_order();
        assert!(source.browse_order.is_some());
        eprintln!(
            "take_browse_order items={count} ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );

        for wait_ms in [0, 60] {
            let source = Source::start(Mode::Files, &root, &config, config.scan_limit);
            std::thread::sleep(Duration::from_millis(wait_ms));
            let start = Instant::now();
            source.retire();
            eprintln!(
                "retire_mid_scan_after_{wait_ms}ms ms={:.3}",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }

        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.source().finish_scan();
        picker.mode = Mode::Files;
        picker.source().finish_scan();
        picker.mode = Mode::Browse;
        picker.browser().enter();
        let start = Instant::now();
        picker.toggle_browse();
        eprintln!(
            "switch_out_of_browse ms={:.3}",
            start.elapsed().as_secs_f64() * 1000.0
        );
    }

    #[test]
    #[ignore = "local performance measurement; set TADORU_BENCH_ROOT and run in release mode"]
    fn benchmark_local_tree() {
        use ratatui::backend::TestBackend;
        use std::time::Instant;
        let root = std::env::var_os("TADORU_BENCH_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap());
        assert!(root.is_dir());
        eprintln!(
            "root={} OS={} arch={} profile={} excludes={:?}",
            root.display(),
            std::env::consts::OS,
            std::env::consts::ARCH,
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            Config::default().exclude
        );
        for mode in [Mode::Dirs, Mode::Files] {
            for trial in 1..=3 {
                let start = Instant::now();
                let mut source = Source::start(
                    mode,
                    &root,
                    &Config::default(),
                    Config::default().scan_limit,
                );
                source.finish_scan();
                assert!(source.scan_error.is_none());
                let scan = start.elapsed();
                while source.matcher.tick(TICK_MS).running {}
                source.refresh_browse_order();
                let ready = start.elapsed();
                let count = source.matcher.snapshot().item_count();
                let filter_start = Instant::now();
                source.set_query("src", false);
                while source.matcher.tick(TICK_MS).running {}
                let filter = filter_start.elapsed();
                eprintln!(
                    "mode={} trial={trial} items={count} scan_ms={:.3} ready_ms={:.3} filter_ms={:.3}",
                    mode.label(),
                    scan.as_secs_f64() * 1000.0,
                    ready.as_secs_f64() * 1000.0,
                    filter.as_secs_f64() * 1000.0
                );
            }
        }
        let start = Instant::now();
        let mut picker = test_picker(root, Mode::Browse);
        picker.browser();
        let load = start.elapsed();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| picker.render(frame.area(), frame))
            .unwrap();
        let mut samples = Vec::new();
        for _ in 0..100 {
            let start = Instant::now();
            terminal
                .draw(|frame| picker.render(frame.area(), frame))
                .unwrap();
            samples.push(start.elapsed());
        }
        samples.sort();
        eprintln!(
            "browse_load_ms={:.3} cached_render_p50_ms={:.3} cached_render_p95_ms={:.3} (TestBackend, excludes terminal I/O and process startup)",
            load.as_secs_f64() * 1000.0,
            samples[49].as_secs_f64() * 1000.0,
            samples[94].as_secs_f64() * 1000.0
        );
    }

    #[test]
    fn icon_color_survives_selection_without_changing_filename_or_background() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        for current in [false, true] {
            let area = Rect::new(0, 0, 24, 1);
            let mut buffer = Buffer::empty(area);
            let item = browse_row(
                "表.xlsx",
                false,
                current,
                false,
                &[0],
                24,
                RowMarks {
                    icons: true,
                    favorite: false,
                    inside: false,
                },
            );
            List::new(vec![item]).render(area, &mut buffer);
            let icon = &buffer[(2, 0)];
            assert_eq!(icon.fg, Color::Indexed(71));
            let filename_x = 2 + icons::prefix("表.xlsx", false, true).width() as u16;
            assert_eq!(buffer[(filename_x, 0)].fg, theme::MATCH.fg.unwrap());
            assert_eq!(
                buffer[(filename_x + 2, 0)].fg,
                if current {
                    theme::CURRENT.fg.unwrap()
                } else {
                    Color::Reset
                }
            );
            if current {
                assert_eq!(icon.bg, theme::CURRENT.bg.unwrap());
            }
        }
    }

    #[test]
    fn browse_icons_preserve_highlights_and_fit_narrow_columns() {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;

        for enabled in [false, true] {
            let area = Rect::new(0, 0, 24, 1);
            let mut buffer = Buffer::empty(area);
            let item = browse_row(
                "日本.rs",
                false,
                false,
                false,
                &[1],
                24,
                RowMarks {
                    icons: enabled,
                    favorite: false,
                    inside: false,
                },
            );
            List::new(vec![item]).render(area, &mut buffer);
            let offset = if enabled {
                icons::prefix("日本.rs", false, true).width()
            } else {
                0
            };
            assert_eq!(buffer[(2 + offset as u16, 0)].symbol(), "日");
            let matched = &buffer[(4 + offset as u16, 0)];
            assert_eq!(matched.symbol(), "本");
            assert_eq!(matched.fg, theme::MATCH.fg.unwrap());
        }

        for width in 2..12 {
            let area = Rect::new(0, 0, 20, 1);
            let mut buffer = Buffer::empty(area);
            let item = browse_row(
                "日本語の名前.rs",
                false,
                false,
                false,
                &[],
                width,
                RowMarks {
                    icons: true,
                    favorite: false,
                    inside: false,
                },
            );
            List::new(vec![item]).render(area, &mut buffer);
            for x in width as u16..area.width {
                assert_eq!(buffer[(x, 0)].symbol(), " ", "overflow at width {width}");
            }
        }
    }

    fn pieces(line: &Line) -> Vec<(String, bool)> {
        line.spans
            .iter()
            .map(|s| {
                (
                    s.content.to_string(),
                    s.style.fg == Some(Color::Indexed(108)),
                )
            })
            .collect()
    }

    #[test]
    fn a_long_place_keeps_its_end_and_its_name_with_the_matches_moved() {
        // Fits: nothing changes.
        assert_eq!(keep_the_end("abc", &[0, 2], 10), ("abc".into(), vec![0, 2]));
        // Cut from the front, one column left for the ellipsis; a hit in the
        // dropped part goes, the rest move with the text.
        let (shown, hits) = keep_the_end("0123456789", &[1, 7, 9], 5);
        assert_eq!(shown, "…6789");
        assert_eq!(hits, [2, 4]);
        // The name of a favorite stays whole; its hits stay put.
        let (shown, hits) = keep_the_end(":work  C:\\a\\b\\work", &[1, 2, 14, 15], 12);
        assert_eq!(shown, ":work  …work");
        assert_eq!(hits, [1, 2, 8, 9]);
    }

    #[test]
    fn matched_letters_are_shared_out_among_the_rows_of_a_path() {
        let sep = std::path::MAIN_SEPARATOR;
        let root = PathBuf::from("r");
        let src = root.join("src");
        let ui = src.join("ui");
        let gauge = ui.join("gauge.rs");
        let mut marks = HashMap::new();
        // The s of src at 0, the u of ui at 4, and g and a at 7 and 8.
        let display = format!("src{sep}ui{sep}gauge.rs");
        spread_marks(&gauge, &display, &[0, 4, 7, 8], &mut marks);
        assert_eq!(marks[&src], [0]);
        assert_eq!(marks[&ui], [0]);
        assert_eq!(marks[&gauge], [0, 1]);

        // A worse match passing through a folder does not repaint it, and a
        // folder with none of the letters gets no entry.
        let x = ui.join("x");
        spread_marks(&x, &format!("src{sep}ui{sep}x"), &[1, 7], &mut marks);
        assert_eq!(marks[&src], [0]);
        assert_eq!(marks[&x], [0]);
        // A folder's own match wins over one passing through it.
        spread_marks(&ui, &format!("src{sep}ui"), &[5], &mut marks);
        assert_eq!(marks[&ui], [1]);
        assert!(!marks.contains_key(&root));
    }

    #[test]
    fn highlights_runs_of_matched_chars() {
        let line = highlight_line(true, "openssl", &[0, 1, 2]);
        assert_eq!(
            pieces(&line),
            vec![
                ("▌ ".into(), false),
                ("ope".into(), true),
                ("nssl".into(), false)
            ]
        );
    }

    #[test]
    fn highlights_scattered_and_multibyte_chars() {
        let line = highlight_line(false, r"ドキュメント\src", &[1, 7]);
        assert_eq!(
            pieces(&line),
            vec![
                ("  ".into(), false),
                ("ド".into(), false),
                ("キ".into(), true),
                ("ュメント\\".into(), false),
                ("s".into(), true),
                ("rc".into(), false)
            ]
        );
    }

    #[test]
    fn no_indices_means_plain_text() {
        let line = highlight_line(false, "docs", &[]);
        assert_eq!(
            pieces(&line),
            vec![("  ".into(), false), ("docs".into(), false)]
        );
    }

    #[test]
    fn fit_trims_by_display_width() {
        assert_eq!(fit("docs", 10), "docs");
        assert_eq!(fit("devsense.composer-php", 10), "devsense.…");
        // Japanese names take two columns per character.
        assert_eq!(fit("画面録画", 8), "画面録画");
        assert_eq!(fit("画面録画", 7), "画面録…");
        assert_eq!(fit("anything", 0), "");
    }

    #[test]
    fn location_spans_bold_the_last_segment() {
        #[cfg(windows)]
        let (path, parent) = (r"C:\Users\example\tadoru", r"C:\Users\example");
        #[cfg(not(windows))]
        let (path, parent) = ("/home/example/tadoru", "/home/example");

        // The pieces still read as the path, so nothing is lost by splitting it.
        let wide = Rect::new(0, 0, 200, 1);
        let (spans, clickable) = search_location(Path::new(path), wide);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, path);

        // Every step of a path that fits can be clicked, in the search modes
        // as well as in browse, which is how a scan moves up a level. Counted
        // from the steps themselves rather than from the text: on Linux the
        // root step is a lone "/", which reads like a separator but is not.
        let steps: Vec<PathBuf> = crumb_spans(Path::new(path))
            .into_iter()
            .filter_map(|(_, dir)| dir)
            .collect();
        let targets: Vec<PathBuf> = clickable.iter().map(|(_, dir)| dir.clone()).collect();
        assert_eq!(targets, steps);
        let narrow = search_location(Path::new(path), Rect::new(0, 0, 4, 1)).1;
        assert!(narrow.len() < clickable.len());

        // Only the folder in view is emphasised; the trail stays quiet.
        let bold: Vec<String> = spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(bold, vec!["tadoru".to_string()]);

        // Every step points at the directory it names, so a click can go there.
        let targets: Vec<PathBuf> = crumb_spans(Path::new(path))
            .into_iter()
            .filter_map(|(_, dir)| dir)
            .collect();
        assert_eq!(targets.last().unwrap(), Path::new(path));
        assert_eq!(targets[targets.len() - 2], Path::new(parent));
        assert!(targets.first().unwrap().parent().is_none());
    }

    #[test]
    fn shift_tab_steps_through_the_searches_and_skips_browse() {
        assert_eq!(next_search(Mode::Dirs), Mode::Files);
        assert_eq!(next_search(Mode::Files), Mode::Dirs);
        assert_eq!(first_search(Mode::Files), Mode::Files);
        assert_eq!(first_search(Mode::Browse), Mode::Dirs);
        assert_eq!(first_search(Mode::Recent), Mode::Dirs);
    }

    #[test]
    fn the_tab_hints_stay_on_a_narrow_screen() {
        let root = crate::testing::temp_dir().join(format!("tadoru-hints-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for width in [80u16, 120] {
            for (mode, wanted) in [(Mode::Dirs, "Tab: browse"), (Mode::Browse, "Tab: dirs")] {
                let mut picker = test_picker(root.clone(), mode);
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
                terminal
                    .draw(|frame| picker.render(frame.area(), frame))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let bottom: String = (0..width).map(|x| buffer[(x, 11)].symbol()).collect();
                // Whole, and first: what does not fit is left off the end
                // rather than cut from the start of the line. Nothing but
                // border comes before it, however much or little of that
                // the hints leave room for.
                let (before, _) = bottom
                    .split_once(&format!(" {wanted}  "))
                    .unwrap_or_else(|| panic!("{width} {mode:?}: {bottom}"));
                assert!(
                    before.chars().all(|ch| ch == '╰' || ch == '─'),
                    "{width} {mode:?}: {bottom}"
                );
            }
        }
        crate::testing::remove_tree(&root);
    }

    #[test]
    fn tab_goes_between_the_search_and_browse() {
        let root = crate::testing::temp_dir().join(format!("tadoru-tab-{}", std::process::id()));
        std::fs::create_dir_all(root.join("inner")).unwrap();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        // The key hints on the bottom border, wide enough not to be cut.
        let hints = |picker: &mut Picker| {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 12)).unwrap();
            terminal
                .draw(|frame| picker.render(frame.area(), frame))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..160)
                .map(|x| buffer[(x, 11)].symbol())
                .collect::<String>()
        };

        // Opened as cf: Tab pairs browse with files, not with dirs, so a
        // file search is not lost to a look around.
        let mut picker = test_picker(root.clone(), Mode::Files);
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Browse);
        // Browse says where Tab goes back to.
        let shown = hints(&mut picker);
        assert!(shown.contains("Tab: files"), "{shown}");
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Files);
        // The search names both ways out: browse, and the next search.
        let shown = hints(&mut picker);
        assert!(shown.contains("Tab: browse"), "{shown}");
        assert!(shown.contains("S-Tab: dirs"), "{shown}");

        // Shift-Tab picks the other search, and Tab then pairs browse with that.
        picker.handle_key(back_tab);
        assert_eq!(picker.mode, Mode::Dirs);
        picker.handle_key(back_tab);
        assert_eq!(picker.mode, Mode::Files);
        picker.handle_key(back_tab);
        assert_eq!(picker.mode, Mode::Dirs, "Shift-Tab stopped in browse");
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Browse);
        // In browse, Shift-Tab switches between the columns and the tree,
        // and each names where it leads.
        assert!(hints(&mut picker).contains("S-Tab: tree"));
        picker.handle_key(back_tab);
        assert!(picker.in_tree(), "Shift-Tab did not open the tree");
        assert!(hints(&mut picker).contains("S-Tab: browse"));
        picker.handle_key(back_tab);
        assert!(picker.mode == Mode::Browse && !picker.tree_view);
        // Tab still goes back to the search, from either.
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Dirs);

        // Opened straight into browse, Tab starts the directory search.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        let shown = hints(&mut picker);
        assert!(shown.contains("Tab: dirs"), "{shown}");
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Dirs);

        // A click on a tab goes to that mode, and a search reached that way
        // is the one Tab comes back to.
        let mut picker = test_picker(root.clone(), Mode::Dirs);
        picker.switch_to(Mode::Files);
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Browse);
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Files);

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn list_dir_puts_directories_first() {
        let tmp = std::env::temp_dir().join(format!("tadoru-preview-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("zeta")).unwrap();
        std::fs::write(tmp.join("Alpha.txt"), "").unwrap();
        std::fs::write(tmp.join("beta.txt"), "").unwrap();
        let names = list_dir(&tmp);
        std::fs::remove_dir_all(&tmp).unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            names,
            vec![
                format!("zeta{sep}"),
                "Alpha.txt".to_string(),
                "beta.txt".to_string()
            ]
        );
    }
}
