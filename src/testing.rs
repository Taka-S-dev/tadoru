//! Helpers shared by the unit tests.

use std::path::PathBuf;

/// The temporary directory with any 8.3 short components resolved.
///
/// Some Windows hosts put a short path in `TEMP` (`C:\Users\RUNNER~1\...`)
/// while a child process reports the long form of the same directory, so a
/// test that compares the two literally fails there and nowhere else.
pub fn temp_dir() -> PathBuf {
    let temp = std::env::temp_dir();
    let Ok(canonical) = std::fs::canonicalize(&temp) else {
        return temp;
    };
    // canonicalize returns a verbatim path on Windows; the prefix would then
    // differ from every path the tests build by hand.
    let text = canonical.to_string_lossy().into_owned();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
}

/// Delete a fixture tree, waiting out a scanner that has not stopped yet.
///
/// Discarded scans are released in the background, so on Windows a walk of
/// the tree can still hold a handle for a moment after the picker moved on.
/// Deleting a directory in that state fails, and only in a test: tadoru never
/// removes a directory itself.
pub fn remove_tree(path: &std::path::Path) {
    for attempt in 0..50 {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && attempt > 0 => return,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    panic!("could not remove {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_is_an_existing_directory_without_a_verbatim_prefix() {
        let dir = temp_dir();
        assert!(dir.is_dir(), "{} is not a directory", dir.display());
        assert!(!dir.to_string_lossy().starts_with(r"\\?\"));
    }
}
