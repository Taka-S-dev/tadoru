use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use ignore::{DirEntry, WalkBuilder, WalkState};
use nucleo::Injector;

use crate::Mode;

/// One candidate. `display` is what is matched against and shown: the path
/// relative to the scan root, or the whole path for the modes that read a
/// saved list.
///
/// The full path is rebuilt from `display` rather than stored beside it.
/// Keeping both meant a second string and a second allocation for every file
/// found, which on a tree of 300,000 came to around 39 MiB.
#[derive(Clone)]
pub struct Entry {
    pub display: String,
    /// Shared with every entry of the same scan, so this costs one pointer.
    root: Arc<PathBuf>,
    /// Held only for a name that `display` cannot be turned back into. A path
    /// that is not valid UTF-8 loses bytes on the way to a String, and cd-ing
    /// to a lossy rendering of it would silently land somewhere else.
    exact: Option<Box<PathBuf>>,
}

impl Entry {
    pub fn path(&self) -> PathBuf {
        if let Some(exact) = &self.exact {
            return (**exact).clone();
        }
        let shown = Path::new(&self.display);
        if shown.is_absolute() {
            shown.to_path_buf()
        } else {
            self.root.join(shown)
        }
    }

    /// Whether this entry had to keep the real path because `display` is a
    /// lossy rendering of it. Used by the memory benchmark.
    #[cfg(test)]
    pub fn exact_path_kept(&self) -> bool {
        self.exact.is_some()
    }

    /// The entry as it is written to stdout: files hand back the folder that
    /// holds them, since the point is to cd there.
    pub fn output_path(&self, mode: Mode) -> PathBuf {
        let path = self.path();
        match mode {
            Mode::Files => path.parent().map(PathBuf::from).unwrap_or(path),
            _ => path,
        }
    }

    /// An entry for a path no scan produced, such as the folder browse is
    /// sitting in when Enter is pressed.
    pub fn absolute(path: PathBuf) -> Self {
        let shown = path.to_string_lossy();
        Self::new(&Arc::new(PathBuf::new()), &path, shown)
    }

    fn new(root: &Arc<PathBuf>, path: &Path, shown: std::borrow::Cow<'_, str>) -> Self {
        Self {
            // A borrowed rendering came back unchanged, so the path can be
            // rebuilt from it; an owned one had bytes replaced.
            exact: matches!(shown, std::borrow::Cow::Owned(_))
                .then(|| Box::new(path.to_path_buf())),
            display: shown.into_owned(),
            root: root.clone(),
        }
    }
}

/// The flags a walk shares with the picker that started it.
#[derive(Clone)]
pub struct Signals {
    /// Set when the thread is finished, whether it walked everything or not.
    pub done: Arc<AtomicBool>,
    /// Set by the picker to stop a walk it no longer needs.
    pub cancel: Arc<AtomicBool>,
    /// Set when the walk stopped at the configured ceiling.
    pub truncated: Arc<AtomicBool>,
}

/// Walks `root` on background threads, pushing entries into `injector` as they are found.
/// `.gitignore` files are not honoured: build outputs are valid destinations too.
/// Setting `cancel` stops the walk early; `done` is set when the thread is finished either way.
pub fn spawn(
    root: PathBuf,
    mode: Mode,
    exclude: &[String],
    limit: usize,
    injector: Injector<Entry>,
    signals: Signals,
) -> JoinHandle<Result<Vec<u32>, String>> {
    let Signals {
        done,
        cancel,
        truncated,
    } = signals;
    if matches!(mode, Mode::Recent | Mode::Favorites) {
        return std::thread::spawn(move || {
            // zoxide's score order is the point of recent mode, and favorites
            // keep the order they were saved in, so neither is re-sorted.
            let result = push_saved(&injector, mode);
            done.store(true, Ordering::Release);
            result.map(|()| Vec::new())
        });
    }
    let exclude: Arc<HashSet<OsString>> = Arc::new(exclude.iter().map(OsString::from).collect());
    // One copy of the root for every entry to point at.
    let root = Arc::new(root);
    // Zero means no ceiling, which the counter expresses as one it cannot
    // reach rather than as a branch on every entry.
    let limit = if limit == 0 { usize::MAX } else { limit };
    let found = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::spawn(move || {
        if let Err(error) = check_scan_root(&root) {
            done.store(true, Ordering::Release);
            return Err(error);
        }
        // Sort keys are built here, beside the walk that already owns every
        // name. Building them from the finished list instead put around
        // 90 ms on the thread that draws, every time a scan completed.
        let keys: Arc<Mutex<Vec<(usize, String, u32)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut builder = WalkBuilder::new(root.as_path());
        builder
            .standard_filters(false)
            .follow_links(false)
            .threads(std::thread::available_parallelism().map_or(4, |n| n.get()));
        builder.filter_entry(move |entry| {
            !skip_reparse_directory(entry)
                && !(entry.file_type().is_some_and(|t| t.is_dir())
                    && exclude.contains(entry.file_name()))
        });

        builder.build_parallel().run(|| {
            let root = root.clone();
            let injector = injector.clone();
            let cancel = cancel.clone();
            // Each worker fills its own vector and hands it over when the walk
            // drops its closure, so the shared lock is taken once per thread
            // rather than once per file.
            let mut mine = Worker {
                keys: Vec::new(),
                shared: keys.clone(),
            };
            let found = found.clone();
            let truncated = truncated.clone();
            Box::new(move |entry| {
                if cancel.load(Ordering::Relaxed) {
                    return WalkState::Quit;
                }
                if let Ok(entry) = entry
                    && let Some(item) = to_entry(&root, &entry, mode)
                {
                    // Counted before the push, so the workers between them
                    // cannot overshoot the ceiling by more than their number.
                    if found.fetch_add(1, Ordering::Relaxed) >= limit {
                        truncated.store(true, Ordering::Release);
                        return WalkState::Quit;
                    }
                    let depth = item.display.matches(MAIN_SEPARATOR).count();
                    let sort_key = item.display.to_lowercase();
                    let index =
                        injector.push(item, |item, cols| cols[0] = item.display.as_str().into());
                    mine.keys.push((depth, sort_key, index));
                }
                WalkState::Continue
            })
        });
        let mut keys = std::mem::take(&mut *keys.lock().expect("scan keys"));
        keys.sort_unstable();
        done.store(true, Ordering::Release);
        Ok(keys.into_iter().map(|(_, _, index)| index).collect())
    })
}

/// Hands a worker's sort keys to the shared vector when the walk is done with
/// it. `build_parallel` has no per-thread finish hook, so this rides on Drop.
struct Worker {
    keys: Vec<(usize, String, u32)>,
    shared: Arc<Mutex<Vec<(usize, String, u32)>>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.append(&mut self.keys);
        }
    }
}

#[cfg(windows)]
fn check_scan_root(root: &Path) -> Result<(), String> {
    check_windows_volume(root)?;
    // Resolve a local junction used as the scan root before walking its target.
    match std::fs::canonicalize(root) {
        Ok(resolved) => check_windows_volume(&resolved),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Cannot verify scan location: {error}. Use browse instead."
        )),
    }
}

#[cfg(windows)]
fn check_windows_volume(root: &Path) -> Result<(), String> {
    use std::path::{Component, Prefix};
    let absolute = std::path::absolute(root).map_err(|error| error.to_string())?;
    let drive = match absolute.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(
                "Recursive scan blocked on network or unsupported paths. Use browse (Tab) instead."
                    .into(),
            ),
        },
        _ => return Err("Cannot verify scan drive. Use browse (Tab) instead.".into()),
    };
    let name = [drive as u16, b':' as u16, b'\\' as u16, 0];
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    // SAFETY: name is a valid, NUL-terminated UTF-16 drive root for this call.
    match unsafe { GetDriveTypeW(name.as_ptr()) } {
        2 | 3 | 5 | 6 => Ok(()),
        _ => Err(
            "Recursive scan blocked on network or unverified drives. Use browse (Tab) instead."
                .into(),
        ),
    }
}

#[cfg(not(windows))]
fn check_scan_root(_root: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(windows)]
fn skip_reparse_directory(entry: &DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    entry.depth() > 0
        && entry.file_type().is_some_and(|kind| kind.is_dir())
        && entry
            .metadata()
            .map_or(true, |metadata| metadata.file_attributes() & 0x400 != 0)
}

#[cfg(not(windows))]
fn skip_reparse_directory(_entry: &DirEntry) -> bool {
    false
}

#[cfg(all(test, windows))]
mod network_tests {
    use super::*;

    #[test]
    fn unc_paths_are_blocked_without_contacting_the_server() {
        for path in [
            r"\\unreachable.invalid\share",
            r"\\?\UNC\unreachable.invalid\share",
            "//unreachable.invalid/share",
        ] {
            assert!(
                check_scan_root(Path::new(path))
                    .unwrap_err()
                    .contains("blocked")
            );
        }
        assert!(check_scan_root(&std::env::current_dir().unwrap()).is_ok());
    }
}

fn to_entry(root: &Arc<PathBuf>, entry: &DirEntry, mode: Mode) -> Option<Entry> {
    if entry.depth() == 0 {
        return None;
    }
    let is_dir = entry.file_type()?.is_dir();
    let wanted = match mode {
        Mode::Dirs => is_dir,
        Mode::Files => !is_dir,
        Mode::Recent | Mode::Favorites | Mode::Browse => false,
    };
    if !wanted {
        return None;
    }
    let relative = entry.path().strip_prefix(&**root).ok()?;
    Some(Entry::new(root, entry.path(), relative.to_string_lossy()))
}

/// Recent directories in zoxide's order (highest score first). zoxide owns the
/// history; reading its database directly would tie tadoru to its file format.
pub fn recent() -> Result<Vec<PathBuf>, String> {
    let output = std::process::Command::new("zoxide")
        .args(["query", "--list"])
        .output()
        .map_err(|err| match err.kind() {
            // Saying only that a program is missing leaves the reader to work
            // out which parts of tadoru still work and what to do about it.
            std::io::ErrorKind::NotFound => {
                "recent mode lists the directories zoxide remembers, and zoxide is not installed. \
                 Install it (winget install ajeetdsouza.zoxide), or use favorites for the places \
                 you care about: Ctrl-B pins the selected folder. dirs, files and browse need nothing."
                    .to_string()
            }
            _ => format!("cannot run zoxide: {err}"),
        })?;
    if !output.status.success() {
        return Err(format!(
            "zoxide query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn push_saved(injector: &Injector<Entry>, mode: Mode) -> Result<(), String> {
    let paths = if mode == Mode::Favorites {
        crate::favorites::path()
            .and_then(|path| crate::favorites::read(&path))
            .map_err(|error| error.to_string())?
    } else {
        recent()?
    };
    // A saved list is not relative to anything, so the whole path is shown
    // and the root goes unused.
    let root = Arc::new(PathBuf::new());
    for path in paths {
        let entry = Entry::new(&root, &path, path.to_string_lossy());
        injector.push(entry, |item, cols| cols[0] = item.display.as_str().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebuilt_path_matches_and_a_lossy_name_keeps_the_real_one() {
        use std::borrow::Cow;
        let root = Arc::new(PathBuf::from("root"));
        let real = root.join("sub").join("file.txt");

        // The ordinary case: nothing is stored but the relative name, and the
        // path comes back whole.
        let plain = Entry::new(&root, &real, Cow::Borrowed("sub/file.txt"));
        assert!(!plain.exact_path_kept());
        assert_eq!(plain.path(), Path::new("root").join("sub").join("file.txt"));
        assert_eq!(
            plain.output_path(Mode::Files),
            Path::new("root").join("sub")
        );

        // A name that lost bytes on the way to a String cannot be turned back
        // into the path, so cd would land somewhere else. That one is kept.
        let lossy = Entry::new(&root, &real, Cow::Owned("sub/fi\u{fffd}e.txt".into()));
        assert!(lossy.exact_path_kept());
        assert_eq!(lossy.path(), real);

        // A saved list holds whole paths, which must not be joined to a root.
        let saved = Entry::absolute(real.clone());
        assert_eq!(saved.path(), real);
    }
}
