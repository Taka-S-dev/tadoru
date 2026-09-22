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
/// How long Backspace has to rest before a press counts as a new one. It only
/// matters where letting go of a key is not reported, which is everywhere but
/// Windows. A held key whose repeat starts later than this is taken for a new
/// press, and goes up once the filter is empty.
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
    /// Whether the Backspace being held, or pressed in quick succession, has
    /// deleted something. It stops at an empty filter instead of going up.
    erase_deleted: bool,
    /// When Backspace last arrived, for terminals that report no releases.
    erase_at: Option<std::time::Instant>,
    notice: Option<String>,
    pinned: crate::favorites::Index,
    menu: Option<crate::action_menu::Menu>,
    /// The panel of keys that Ctrl-Space opens.
    keys: Option<crate::key_menu::KeyMenu>,
    /// The last mode that shows a place: dirs, files or browse. Recent and
    /// favorites list places to go, and Right in them goes back here.
    origin: Mode,
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
    let mut picker = Picker {
        sources: std::array::from_fn(|_| None),
        browser: (args.mode == Mode::Browse).then(|| Browser::new(root.clone())),
        browser_root: root.clone(),
        search_mode: first_search(args.mode),
        mode: args.mode,
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
        erase_deleted: false,
        erase_at: None,
        notice,
        pinned,
        menu: None,
        keys: None,
        tree_view: false,
        tree: None,
        origin: place_mode(args.mode),
        mouse_rows: (Rect::default(), 0, 0),
        mouse_header: Rect::default(),
        mouse_nav: Vec::new(),
        mouse_crumbs: Vec::new(),
        mouse_modes_x: 0,
        mouse_paths: Vec::new(),
        last_click: None,
    };
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

    let result = picker.run_tui();
    let mode = picker.mode;
    drop(picker);
    result.map(|entry| entry.map(|e| e.output_path(mode)))
}

fn mode_index(mode: Mode) -> usize {
    MODE_ORDER
        .iter()
        .position(|&m| m == mode)
        .expect("known mode")
}

/// Backspace, or Ctrl-H standing in for it.
fn is_erase(key: &KeyEvent) -> bool {
    key.code == KeyCode::Backspace
        || (key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Where Right leads from a picker opened in `mode`. Opened straight into recent
/// or favorites there is nowhere to go back to, so the place is browsed.
fn place_mode(mode: Mode) -> Mode {
    match mode {
        Mode::Recent | Mode::Favorites => Mode::Browse,
        place => place,
    }
}

/// The search a picker opened in `mode` goes back to from browse. One opened
/// straight into browse has not searched yet, so it gets the directory search.
fn first_search(mode: Mode) -> Mode {
    if mode == Mode::Browse {
        Mode::Dirs
    } else {
        mode
    }
}

/// The search after `mode` in tab order, for Shift-Tab. Browse is where Tab
/// goes rather than another kind of search, so it is passed over.
fn next_search(mode: Mode) -> Mode {
    let start = mode_index(mode);
    (1..MODE_ORDER.len())
        .map(|step| MODE_ORDER[(start + step) % MODE_ORDER.len()])
        .find(|&m| m != Mode::Browse)
        .expect("a search mode")
}

impl Picker {
    /// The current mode's source, started on first use.
    fn source(&mut self) -> &mut Source {
        let idx = mode_index(self.mode);
        if self.sources[idx].is_none() {
            let limit = if self.unlimited {
                0
            } else {
                self.config.scan_limit
            };
            let mut source = Source::start(self.mode, &self.root, &self.config, limit);
            source.set_query(&self.query, false);
            self.sources[idx] = Some(source);
        }
        self.sources[idx].as_mut().expect("just created")
    }

    fn set_query(&mut self, query: &str) {
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
            let select = self.browser().selected_path();
            self.tree = Some(crate::tree::Tree::new(root, select.as_deref()));
        }
        self.tree.as_mut().expect("just built")
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
            let selected = self.tree.as_ref().and_then(|tree| tree.selected_path());
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
        if is_erase(&key) {
            // Nothing is typed in the tree yet, so Backspace goes up as
            // it does in the columns once the filter is empty.
            if self.erase(false) {
                self.tree_left();
            }
            return Action::Continue;
        }
        match (key.code, ctrl) {
            (KeyCode::Left, _) => self.tree_left(),
            (KeyCode::Right, _) | (KeyCode::Char('l'), true) => self.tree().right(),
            (KeyCode::Up, _) | (KeyCode::Char('k'), true) => self.tree().move_selection(-1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), true) => self.tree().move_selection(1),
            (KeyCode::PageUp, _) => self.tree().move_selection(-10),
            (KeyCode::PageDown, _) => self.tree().move_selection(10),
            _ => {}
        }
        Action::Continue
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

    /// Right on a search result. From dirs and files that is a look inside, in
    /// browse. Recent and favorites are lists of places to go rather than
    /// somewhere to be, so from them it goes back to the mode they were opened
    /// from, now at the place chosen: a search that was under way carries on
    /// from the favorite.
    ///
    /// Going back to browse, the place is shown in its folder and selected,
    /// as Tab shows a search result, rather than stepped into. Inside it the
    /// highlight would be on the first row, which nobody chose, and Enter
    /// would cd there instead of to the favorite.
    fn go_into(&mut self, path: &Path) {
        let listed = matches!(self.mode, Mode::Recent | Mode::Favorites);
        let origin = self.origin;
        if listed && origin == Mode::Browse {
            self.mode = Mode::Browse;
            self.browser().reveal(path);
            self.preview = None;
            return;
        }
        self.browse_at(path);
        if listed {
            self.switch_to(origin);
            // What was typed picked the place out and would match nothing
            // inside it. Going back brings it back with where it belonged.
            self.set_query("");
        }
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

    /// Shift-Tab: the next kind of search. Browse goes back to its search
    /// instead, as Tab does, so the key never leaves the reader in browse.
    fn next_search_mode(&mut self) {
        let target = if self.mode == Mode::Browse {
            self.search_mode
        } else {
            next_search(self.mode)
        };
        self.switch_to(target);
    }

    /// First entry into browse uses the search selection; subsequent mode
    /// switches restore the existing browser, including its filter and selection.
    fn switch_to(&mut self, entering: Mode) {
        let leaving = self.mode;
        if entering == leaving {
            return;
        }
        if entering != Mode::Browse {
            self.search_mode = entering;
        }
        if !matches!(leaving, Mode::Recent | Mode::Favorites) {
            self.origin = leaving;
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
        if entering == Mode::Favorites {
            self.reload_pinned();
            if let Some(stale) = self.sources[mode_index(Mode::Favorites)].take() {
                stale.retire();
            }
        }
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
                Event::Resize(_, rows) => {
                    terminal.reopen(rows)?;
                    continue;
                }
                _ => continue,
            };
            if key.kind == KeyEventKind::Release {
                if is_erase(&key) {
                    self.end_erase();
                }
                continue;
            }
            match self.handle_key(key) {
                Action::Continue => {}
                Action::Cancel => return Ok(None),
                Action::Execute(request) => {
                    let (action, target) = *request;
                    let name = action.name().to_string();
                    if action.run_mode() == crate::actions::RunMode::Terminal {
                        let resume_top = terminal.get_frame().area().y;
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
                        terminal = TerminalGuard::enter_at(Some(resume_top), self.config.mouse)?;
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
                }
                Action::Accept => {
                    if self.mode == Mode::Browse {
                        let target = if self.tree_view {
                            self.tree().target()
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
        if self.in_tree() {
            return self.tree().selected_path();
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
            for (mode, start, end) in tab_offsets() {
                if mouse.column >= first + start && mouse.column < first + end {
                    self.switch_to(mode);
                    return;
                }
            }
        }
        let (area, first, count) = self.mouse_rows;
        if !area.contains(position) || count == 0 {
            return;
        }
        let selected = if self.in_tree() {
            self.tree().selected
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
            self.tree().selected = next;
            if mouse.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some(path) = self.tree().selected_path()
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
                if double {
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
            self.menu = Some(crate::action_menu::Menu::new(target));
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

    /// One press of Backspace or Ctrl-H, given whether there is anything left
    /// to delete. Says whether it should go up instead.
    ///
    /// Holding the key to clear what was typed must stop once it is clear:
    /// a run of presses that began by deleting does not carry on upwards. A
    /// run ends when the key is let go, when another key comes, or, where
    /// the terminal reports no releases, after a pause.
    fn erase(&mut self, has_text: bool) -> bool {
        let now = std::time::Instant::now();
        if self
            .erase_at
            .is_some_and(|at| now.duration_since(at) > ERASE_PAUSE)
        {
            self.erase_deleted = false;
        }
        self.erase_at = Some(now);
        if has_text {
            self.erase_deleted = true;
            return false;
        }
        !self.erase_deleted
    }

    fn end_erase(&mut self) {
        self.erase_deleted = false;
        self.erase_at = None;
    }

    /// Starts ignoring clicks, because what is under the pointer has just been
    /// replaced and the next one is most likely a leftover from the old screen.
    fn block_clicks(&mut self) {
        self.clicks_blocked_until = Some(std::time::Instant::now() + CLICK_GUARD);
        self.last_click = None;
    }

    /// Whether a button press should be discarded as belonging to the screen
    /// that was on show a moment ago.
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
        if let Some(menu) = &mut self.menu {
            return match menu.handle(key) {
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
        if !is_erase(&key) {
            self.end_erase();
        }
        // Some terminals send Ctrl-Space as a bare NUL.
        if key.code == KeyCode::Null
            || (key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::CONTROL)
        {
            self.keys = Some(crate::key_menu::KeyMenu::new(
                self.mode,
                self.origin,
                self.in_tree(),
            ));
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
                self.menu = Some(crate::action_menu::Menu::new(target));
                return Action::Continue;
            }
            (KeyCode::Char('b'), true) => {
                let target = if self.in_tree() {
                    Some(self.tree().target()).filter(|path| !path.as_os_str().is_empty())
                } else if self.mode == Mode::Browse {
                    Some(self.browser().target()).filter(|path| !path.as_os_str().is_empty())
                } else {
                    let mode = self.mode;
                    self.source()
                        .selected_entry()
                        .map(|entry| entry.output_path(mode))
                };
                let message = match target {
                    None => "Nothing selected. Switch to browse to pin this folder.".into(),
                    Some(path) => match crate::favorites::path().and_then(|file| {
                        let added = crate::favorites::update(&file, &path, None)?;
                        crate::favorites::Index::read(&file).map(|index| (added, index))
                    }) {
                        Ok((added, index)) => {
                            self.pinned = index;
                            self.sources[mode_index(Mode::Favorites)] = None;
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
                let has_filter = if self.mode == Mode::Browse {
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
            // Ctrl-H is Backspace: it is what some terminals send for the
            // Backspace key itself. With nothing left to delete it widens the
            // search as Left does, as browse climbs.
            (KeyCode::Backspace, _) | (KeyCode::Char('h'), true) => {
                if self.erase(!self.query.is_empty()) {
                    self.navigate(Nav::Up);
                } else if !self.query.is_empty() {
                    let mut q = self.query.clone();
                    q.pop();
                    self.set_query(&q);
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
            (KeyCode::Left, _) => {
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
        // Backspace on an empty filter climbs, as in yazi, and Ctrl-H is
        // Backspace here as in the search.
        if is_erase(&key) {
            let has_text = !self.browser().filter.is_empty();
            if self.erase(has_text) {
                self.browser().up();
            } else if has_text {
                let b = self.browser();
                let mut f = b.filter.clone();
                f.pop();
                b.set_filter(&f);
            }
            return Action::Continue;
        }
        let b = self.browser();
        match (key.code, ctrl) {
            (KeyCode::Left, _) => b.up(),
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
        if let Some(menu) = &mut self.menu {
            menu.render(area, frame);
        }
        if let Some(keys) = &mut self.keys {
            keys.render(area, frame);
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

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::BORDER)
            .title(Line::from(mode_tabs(mode)))
            // Shift-Tab is named by where it leads, as Tab is in browse.
            .title_bottom(footer_line(
                self.notice.as_deref(),
                &fit_hints(
                    &[
                        "Tab: browse",
                        "^Space: keys",
                        &format!("S-Tab: {}", next_search(mode).label()),
                        "Enter: cd",
                        // Named by where it leads, which from a list of
                        // places is wherever that list was opened from.
                        &match (mode, self.origin) {
                            (Mode::Recent | Mode::Favorites, Mode::Dirs | Mode::Files) => {
                                format!("Right: {} from it", self.origin.label())
                            }
                            _ => "Right: browse it".to_string(),
                        },
                        "Esc: clear/exit",
                        "^P: actions",
                        "^B: pin",
                        "F5: refresh",
                    ],
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
        let (spans, crumbs) = match mode {
            Mode::Recent => (vec![Span::styled("zoxide", theme::HEADER)], Vec::new()),
            Mode::Favorites => (
                vec![Span::styled("pinned directories", theme::HEADER)],
                Vec::new(),
            ),
            // The path a scan started from is also the way out of it: clicking
            // a step above the current one searches from there instead.
            _ => search_location(&root, rest),
        };
        header.extend(spans);
        self.mouse_crumbs = crumbs;
        frame.render_widget(Paragraph::new(Line::from(header)), header_area);

        if let Some(error) = &source.scan_error {
            frame.render_widget(
                Paragraph::new(format!(
                    "Cannot load {}: {error}\nF5: retry  Shift-Tab: other list  Esc: cancel",
                    mode.label()
                ))
                .style(Style::default().fg(Color::Red))
                .wrap(Wrap { trim: false }),
                rows_area,
            );
            return;
        }
        if mode == Mode::Recent && !scanning && total == 0 {
            frame.render_widget(
                Paragraph::new("No history yet. F5: refresh  Shift-Tab: other list"),
                rows_area,
            );
        }
        if mode == Mode::Favorites && !scanning && total == 0 {
            frame.render_widget(
                Paragraph::new(
                    "No favorites yet. Ctrl-B: pin a folder in another mode. Shift-Tab: other list",
                )
                .wrap(Wrap { trim: false }),
                rows_area,
            );
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
            let dir = self.tree().selected_path().filter(|p| p.is_dir());
            self.render_preview(preview_area, frame, dir.as_deref());
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::BORDER)
            .title(Line::from(mode_tabs(Mode::Browse)))
            .title_bottom(footer_line(
                self.notice.as_deref(),
                &fit_hints(
                    &[
                        &format!("Tab: {}", self.search_mode.label()),
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
            Paragraph::new(Span::styled("> ", theme::PROMPT)),
            prompt_area,
        );
        if prompt_area.width > 2 {
            frame.set_cursor_position((prompt_area.x + 2, prompt_area.y));
        }
        let label = " tree ";
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(label, theme::INFO),
                Span::styled(
                    "─".repeat((info_area.width as usize).saturating_sub(label.len())),
                    theme::BORDER,
                ),
            ])),
            info_area,
        );

        self.tree();
        let icons = self.config.icons;
        let tree = self.tree.as_mut().expect("built above");
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
                let style = match (current, node.depth == 0, node.is_dir) {
                    (true, _, _) => theme::CURRENT,
                    (false, true, _) => theme::HERE_PATH,
                    (false, false, true) => theme::DIR,
                    (false, false, false) => Style::default(),
                };
                let pointer = if current {
                    Span::styled("▌ ", theme::POINTER)
                } else {
                    Span::raw("  ")
                };
                ListItem::new(
                    Line::from(vec![
                        pointer,
                        Span::styled(node.guide.clone(), theme::BORDER),
                        icons::span(icon),
                        Span::raw(fit(&node.label, room)),
                    ])
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
            .title(Line::from(mode_tabs(Mode::Browse)))
            // Tab goes back to whichever search was used last, so the hint
            // names it rather than leaving the reader to remember.
            .title_bottom(footer_line(
                self.notice.as_deref(),
                &fit_hints(
                    &[
                        &format!("Tab: {}", self.search_mode.label()),
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
            return self.tree().selected_path().filter(|p| p.is_dir());
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

/// What is drawn before a tab's label, after the one before it.
///
/// The four searches share a bracket and browse has its own: Shift-Tab steps
/// through the searches and Tab goes across to browse, and five tabs in one
/// row read as five of the same kind.
fn tab_separator(mode: Mode) -> &'static str {
    if mode == Mode::Browse { "]  [" } else { "|" }
}

/// Where each tab's label starts and ends, counted in columns from the first
/// label. Drawing and clicking both go by it, so a click cannot land on a
/// different tab from the one drawn there.
fn tab_offsets() -> Vec<(Mode, u16, u16)> {
    let mut offsets = Vec::new();
    let mut x = 0;
    for (i, &m) in MODE_ORDER.iter().enumerate() {
        if i > 0 {
            x += tab_separator(m).len() as u16;
        }
        let end = x + m.label().len() as u16;
        offsets.push((m, x, end));
        x = end;
    }
    offsets
}

/// The tabs, `[dirs|files|recent|favorites]  [browse]`, drawn on the top border.
fn mode_tabs(mode: Mode) -> Vec<Span<'static>> {
    let mut header: Vec<Span> = vec![Span::styled("[", theme::HEADER)];
    for (i, &m) in MODE_ORDER.iter().enumerate() {
        if i > 0 {
            header.push(Span::styled(tab_separator(m), theme::HEADER));
        }
        let style = if m == mode {
            theme::HEADER.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            theme::HEADER
        };
        header.push(Span::styled(m.label(), style));
    }
    header.push(Span::styled("] ", theme::HEADER));
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
    let matched = theme::MATCH;
    let pointer = if current {
        Span::styled("▌ ", theme::POINTER)
    } else {
        Span::raw("  ")
    };
    let mut spans = vec![pointer];
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
    Line::from(spans)
}

/// The whole window.
///
/// Sizing to the space below the cursor left the shell history at the top, so
/// a few lines of it cost the listing rows it could have used. Taking the full
/// height scrolls that history up instead, as `fzf --height 100%` does, and it
/// is still in the scrollback afterwards.
fn inline_height(rows: u16) -> u16 {
    rows.max(1)
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

/// Owns raw mode and the inline viewport, including error cleanup.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
    mouse: bool,
}

impl TerminalGuard {
    /// Opens a viewport over the whole window, like fzf --height 100%, so the
    /// shell history scrolls up rather than eating rows the listing could use.
    fn enter(mouse: bool) -> Result<Self, Error> {
        Self::enter_at(None, mouse)
    }

    fn enter_at(top: Option<u16>, mouse: bool) -> Result<Self, Error> {
        let (_, rows) = crossterm::terminal::size()?;
        if let Some(top) = top {
            let top = top.min(rows.saturating_sub(1));
            crossterm::execute!(io::stderr(), crossterm::cursor::MoveTo(0, top))?;
        }
        enable_raw_mode()?;
        match Self::open(rows) {
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

    /// Creates a viewport covering a `rows`-tall screen.
    fn open(rows: u16) -> Result<Terminal<CrosstermBackend<io::Stderr>>, Error> {
        let height = inline_height(rows);
        let terminal = Terminal::with_options(
            CrosstermBackend::new(io::stderr()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )?;
        Ok(terminal)
    }

    /// Rebuilds the viewport after the window changed size. An inline viewport's
    /// height is fixed when it is created, so growing the window would otherwise
    /// leave the extra rows unused, and shrinking it would draw off screen.
    fn reopen(&mut self, rows: u16) -> Result<(), Error> {
        self.terminal.clear()?;
        crossterm::execute!(io::stderr(), crossterm::cursor::MoveTo(0, 0))?;
        self.terminal = Self::open(rows)?;
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
        // Wipe the viewport so the prompt comes back where the picker opened.
        let top = self.terminal.get_frame().area().y;
        let _ = self.terminal.clear();
        let _ = crossterm::execute!(io::stderr(), crossterm::cursor::MoveTo(0, top));
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
            erase_deleted: false,
            erase_at: None,
            notice: None,
            pinned: crate::favorites::Index::default(),
            menu: None,
            keys: None,
            tree_view: false,
            tree: None,
            origin: place_mode(mode),
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
        // search started from the tree starts at its root. Favorites is used
        // because it scans nothing.
        picker.handle_key(ctrl_space);
        picker.handle_key(key(KeyCode::Char('v')));
        picker.switch_to(Mode::Favorites);
        assert_eq!(picker.root, inner);
        picker.switch_to(Mode::Browse);
        assert!(picker.in_tree());
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

        // The four searches share one bracket; browse, which Tab pairs with
        // them, has its own.
        let mut picker = test_picker(root.clone(), Mode::Browse);
        find(
            &top_row(&mut picker),
            "[dirs|files|recent|favorites]  [browse]",
        );

        // The first and last letter of each label switch to that mode.
        for mode in MODE_ORDER {
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
        assert_eq!(picker.mode, Mode::Favorites);
        assert!(picker.keys.is_none());
        assert_eq!(picker.query, "inner");

        // Esc closes the panel and nothing else: the filter is still there,
        // where Esc on the picker itself would have cleared it.
        picker.handle_key(ctrl_space);
        picker.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(picker.keys.is_none());
        assert_eq!(picker.query, "inner");
        assert_eq!(picker.mode, Mode::Favorites);

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

        for (letter, mode) in [
            ('s', Mode::Favorites),
            ('r', Mode::Recent),
            ('f', Mode::Files),
            ('d', Mode::Dirs),
        ] {
            picker.handle_key(ctrl(letter));
            assert_eq!(picker.mode, mode, "Ctrl-{letter}");
        }
        // From browse, one press instead of Shift-Tab four times over, and
        // Tab from browse then comes back to the search reached this way.
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(picker.mode, Mode::Browse);
        picker.handle_key(ctrl('s'));
        assert_eq!(picker.mode, Mode::Favorites);
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        picker.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(picker.mode, Mode::Favorites);

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
    fn ctrl_h_is_backspace_in_the_search_and_in_browse() {
        let root = crate::testing::temp_dir().join(format!("tadoru-ctrl-h-{}", std::process::id()));
        let inner = root.join("only/inner");
        std::fs::create_dir_all(&inner).unwrap();
        let ctrl_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL);

        // In the search it deletes a letter. Held down, it stops once the
        // filter is empty rather than going on to widen the search.
        let mut picker = test_picker(inner.clone(), Mode::Dirs);
        picker.set_query("ab");
        picker.handle_key(ctrl_h);
        assert_eq!(picker.query, "a");
        assert_eq!(picker.root, inner);
        picker.handle_key(ctrl_h);
        picker.handle_key(ctrl_h);
        assert_eq!(picker.query, "");
        assert_eq!(picker.root, inner, "the held key stops at an empty filter");
        // Let go and pressed again, it widens the search as Left does.
        picker.end_erase();
        picker.handle_key(ctrl_h);
        assert_eq!(picker.root, root.join("only"));
        drop(picker);

        // With nothing typed to begin with, the first press widens: straight
        // after starting, Ctrl-H goes up as it does in browse.
        let mut picker = test_picker(inner.clone(), Mode::Dirs);
        picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(picker.root, root.join("only"));
        drop(picker);

        // In browse it deletes a letter of the filter, and climbs once the
        // filter is empty and the key has been let go. Quick presses on an
        // empty filter climb one level each.
        let mut picker = test_picker(inner.clone(), Mode::Browse);
        picker.browser().set_filter("x");
        picker.handle_key(ctrl_h);
        assert_eq!(picker.browser().filter, "");
        picker.handle_key(ctrl_h);
        assert_eq!(
            picker.browser().cwd,
            inner,
            "the held key stops at an empty filter"
        );
        picker.end_erase();
        picker.handle_key(ctrl_h);
        assert_eq!(picker.browser().cwd, root.join("only"));
        picker.handle_key(ctrl_h);
        assert_eq!(picker.browser().cwd, root);
        drop(picker);

        // Another key in between ends the run as letting go does.
        let mut picker = test_picker(inner.clone(), Mode::Browse);
        picker.browser().set_filter("x");
        picker.handle_key(ctrl_h);
        picker.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        picker.handle_key(ctrl_h);
        assert_eq!(picker.browser().cwd, root.join("only"));
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

        // Favorites and recent are lists of places to go, so from them Right
        // goes back to where they were opened from, now at the place chosen.
        let mut picker = test_picker(root.clone(), Mode::Files);
        picker.switch_to(Mode::Favorites);
        assert_eq!(picker.origin, Mode::Files);
        picker.query = "inn".into();
        picker.go_into(&inner);
        assert_eq!(picker.mode, Mode::Files);
        assert_eq!(picker.root, inner);
        // The filter that found the favorite is not carried into it, and going
        // back restores it along with where the search started.
        assert_eq!(picker.query, "");
        picker.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(picker.root, root);
        assert_eq!(picker.query, "inn");
        // Through recent on the way to favorites, it is still files it came from.
        picker.switch_to(Mode::Recent);
        picker.switch_to(Mode::Favorites);
        assert_eq!(picker.origin, Mode::Files);
        drop(picker);

        // Opened from browse it is browse that carries on, and a picker opened
        // straight into a list has nowhere to go back to, so it browses too.
        // There the favorite is shown in its folder and selected, so Enter
        // goes to it rather than to whatever sorts first inside it.
        for start in [Mode::Browse, Mode::Favorites] {
            let mut picker = test_picker(root.clone(), start);
            picker.switch_to(Mode::Favorites);
            picker.go_into(&inner);
            assert_eq!(picker.mode, Mode::Browse, "{start:?}");
            assert_eq!(picker.browser().cwd, root.join("only"));
            assert_eq!(picker.browser().target(), inner);
            // Right again goes into it.
            picker.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
            assert_eq!(picker.browser().cwd, inner);
            drop(picker);
        }

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
                let mut picker = test_picker(root.clone(), Mode::Recent);
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
                    picker.switch_to(Mode::Recent);
                }
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
                // The way out the screen names takes the reader to the next list.
                picker.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
                assert_eq!(picker.mode, Mode::Favorites);
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
    fn the_picker_takes_the_whole_window_whatever_the_history_above() {
        // Shell history costs the listing no rows: it scrolls up.
        assert_eq!(inline_height(50), 50);
        assert_eq!(inline_height(12), 12);
        // A terminal that reports nothing still gets a row to draw in.
        assert_eq!(inline_height(0), 1);
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
        assert_eq!(next_search(Mode::Files), Mode::Recent);
        assert_eq!(next_search(Mode::Recent), Mode::Favorites);
        assert_eq!(next_search(Mode::Favorites), Mode::Dirs);
        assert_eq!(first_search(Mode::Files), Mode::Files);
        assert_eq!(first_search(Mode::Browse), Mode::Dirs);
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
        assert!(shown.contains("S-Tab: recent"), "{shown}");

        // Shift-Tab picks another search, and Tab then pairs browse with that.
        picker.handle_key(back_tab);
        assert_eq!(picker.mode, Mode::Recent);
        picker.handle_key(back_tab);
        picker.handle_key(back_tab);
        assert_eq!(picker.mode, Mode::Dirs, "Shift-Tab stopped in browse");
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Browse);
        // From browse, Shift-Tab goes back to that search too.
        picker.handle_key(back_tab);
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
        picker.switch_to(Mode::Favorites);
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Browse);
        picker.handle_key(tab);
        assert_eq!(picker.mode, Mode::Favorites);

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
