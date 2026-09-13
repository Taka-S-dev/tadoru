//! Hand a path to the desktop: show it in the file manager, or open a file
//! with whatever the system associates with it. The picker stays open, so a
//! look at the folder can precede the decision where to cd.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Shows `path` in the file manager. A file is revealed inside its folder.
pub fn reveal(path: &Path) -> std::io::Result<()> {
    let (program, args) = reveal_command(path, path.is_dir());
    spawn(program, args)
}

/// Opens a file with its associated application. Directories are ignored:
/// that is what `reveal` is for.
pub fn launch(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let (program, args) = launch_command(path);
    spawn(program, args)
}

fn spawn(program: &str, args: Vec<OsString>) -> std::io::Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(windows)]
fn reveal_command(path: &Path, is_dir: bool) -> (&'static str, Vec<OsString>) {
    // explorer.exe takes the path as one argument, so no quoting is needed
    // even for names with spaces or characters cmd.exe would eat.
    if is_dir {
        ("explorer.exe", vec![path.as_os_str().to_owned()])
    } else {
        let mut select = OsString::from("/select,");
        select.push(path.as_os_str());
        ("explorer.exe", vec![select])
    }
}

#[cfg(windows)]
fn launch_command(path: &Path) -> (&'static str, Vec<OsString>) {
    // Given a file, explorer.exe opens it with the associated program.
    ("explorer.exe", vec![path.as_os_str().to_owned()])
}

#[cfg(target_os = "macos")]
fn reveal_command(path: &Path, is_dir: bool) -> (&'static str, Vec<OsString>) {
    if is_dir {
        ("open", vec![path.as_os_str().to_owned()])
    } else {
        ("open", vec!["-R".into(), path.as_os_str().to_owned()])
    }
}

#[cfg(target_os = "macos")]
fn launch_command(path: &Path) -> (&'static str, Vec<OsString>) {
    ("open", vec![path.as_os_str().to_owned()])
}

#[cfg(not(any(windows, target_os = "macos")))]
fn reveal_command(path: &Path, is_dir: bool) -> (&'static str, Vec<OsString>) {
    let target = if is_dir {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    ("xdg-open", vec![target.as_os_str().to_owned()])
}

#[cfg(not(any(windows, target_os = "macos")))]
fn launch_command(path: &Path) -> (&'static str, Vec<OsString>) {
    ("xdg-open", vec![path.as_os_str().to_owned()])
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn reveal_selects_files_and_opens_directories() {
        let (prog, args) = reveal_command(Path::new(r"C:\a b\c.txt"), false);
        assert_eq!(prog, "explorer.exe");
        assert_eq!(args, vec![OsString::from(r"/select,C:\a b\c.txt")]);
        let (_, args) = reveal_command(Path::new(r"C:\a b"), true);
        assert_eq!(args, vec![OsString::from(r"C:\a b")]);
    }
}
