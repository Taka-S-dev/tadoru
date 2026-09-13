//! One command that puts the shell integration in place.
//!
//! `init` stays the primitive: it prints the integration and touches nothing,
//! because where the line belongs and what has to load before it are the
//! reader's decisions, not this program's. `setup` is the convenience built on
//! top, and it earns the right to edit a startup file by saying what it will
//! write, asking first, and writing a block it can find again.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::Shell;
use crate::names::{Names, Taken};

type Error = Box<dyn std::error::Error>;

/// Wraps what this program owns inside a file it does not. Re-running replaces
/// the block rather than appending a second copy, and removing the tool means
/// deleting from one marker to the other.
const BEGIN: &str = "# >>> tadoru >>>";
const END: &str = "# <<< tadoru <<<";

/// What a run would do, worked out before anything is written.
#[derive(Debug)]
pub struct Plan {
    shell: Shell,
    file: PathBuf,
    line: String,
    /// Other programs on PATH that answer to the same names.
    taken: Vec<Taken>,
    /// The file already carries a block, and its contents are unchanged.
    current: bool,
    /// A block is there but says something else, so it will be replaced.
    replacing: bool,
}

impl Plan {
    pub fn describe(&self) -> String {
        let what = match (self.current, self.replacing) {
            (true, _) => "already set up; nothing to write",
            (_, true) => "replacing the existing tadoru block",
            _ => "appending a tadoru block",
        };
        let mut text = format!(
            "  shell: {}\n  file:  {}\n  what:  {what}\n\n{}",
            self.shell.label(),
            self.file.display(),
            block(&self.line)
                .lines()
                .map(|line| format!("    {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        // Shown before asking, since whether the names are free is part of
        // deciding to write them.
        if !self.taken.is_empty() {
            text.push_str("\n\n  Already on PATH under the same name:");
            for taken in &self.taken {
                text.push('\n');
                text.push_str(&taken.for_functions());
            }
            text.push_str(&format!(
                "\n  In {} the tadoru functions hide these, but other shells still run them.\n  {}",
                self.shell.label(),
                crate::names::RENAME_HINT
            ));
        }
        text
    }

    pub fn is_noop(&self) -> bool {
        self.current
    }
}

fn block(line: &str) -> String {
    format!("{BEGIN}\n{line}\n{END}\n")
}

/// The startup file the shell reads, and the line that belongs in it.
///
/// zoxide defines `z` and `zi` as aliases, and an alias beats a function, so
/// the line has to come after it. Appending puts it there for anyone whose
/// zoxide setup is already in the file.
fn plan_for(shell: Shell, exe: &Path, names: &Names) -> Result<Plan, Error> {
    let exe_dir = exe.parent().map(Path::to_path_buf);
    let exe = exe.display().to_string();
    let (file, line) = match shell {
        Shell::Powershell => (
            powershell_profile()?,
            // Windows PowerShell 5.1 decodes what a program prints using
            // the console code page. On a Japanese system that is Shift_JIS,
            // so an installation path with Japanese in it comes back broken
            // and the function it defines points at nothing. Reading this one
            // line as UTF-8 costs nothing on PowerShell 7, which already does.
            format!(
                "$tadoruEncoding = [Console]::OutputEncoding\n\
                 [Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
                 Invoke-Expression (& '{}' init powershell{} | Out-String)\n\
                 [Console]::OutputEncoding = $tadoruEncoding\n\
                 Remove-Variable tadoruEncoding",
                exe.replace('\'', "''"),
                names.flags()
            ),
        ),
        Shell::Bash => (
            home()?.join(".bashrc"),
            format!(
                "eval \"$({} init bash{})\"",
                exe.replace('\\', "/"),
                names.flags()
            ),
        ),
        Shell::Cmd => {
            return Err(
                "cmd.exe has no startup file to add a line to. Put the .cmd files on PATH instead: tadoru init cmd --out <dir>"
                    .into(),
            );
        }
    };
    let existing = read_existing(&file)?;
    let found = extract(&existing);
    // tadoru's own scripts beside the program are not a collision.
    let skip: Vec<PathBuf> = exe_dir.into_iter().collect();
    Ok(Plan {
        shell,
        file,
        current: found.as_deref() == Some(line.as_str()),
        replacing: found.is_some(),
        taken: crate::names::taken(names, &skip, None),
        line,
    })
}

/// The startup file as it stands, or nothing when there is no file yet.
///
/// A file that exists but cannot be read as UTF-8 stops the run. Treating it
/// as empty would replace someone's whole startup file with our block.
fn read_existing(file: &Path) -> Result<String, Error> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(format!(
            "{}: {error}. Add the block by hand rather than have this overwrite the file",
            file.display()
        )
        .into()),
    }
}

/// The line inside the block, if the file already has one.
fn extract(text: &str) -> Option<String> {
    let start = text.find(BEGIN)? + BEGIN.len();
    let end = text[start..].find(END)? + start;
    Some(text[start..end].trim().to_string())
}

pub fn plan(shell: Option<Shell>, names: &Names) -> Result<Plan, Error> {
    let shell = match shell {
        Some(shell) => shell,
        None => detect(),
    };
    plan_for(shell, &std::env::current_exe()?, names)
}

/// The shell that launched this process, falling back to the platform default.
fn detect() -> Shell {
    // Set by the PowerShell shim and by pwsh itself for child processes.
    if std::env::var_os("PSModulePath").is_some() {
        return Shell::Powershell;
    }
    if std::env::var_os("BASH").is_some() || std::env::var_os("SHELL").is_some() {
        return Shell::Bash;
    }
    if cfg!(windows) {
        Shell::Powershell
    } else {
        Shell::Bash
    }
}

/// Writes the block, replacing one already there.
pub fn apply(plan: &Plan) -> Result<(), Error> {
    if plan.current {
        return Ok(());
    }
    let existing = read_existing(&plan.file)?;
    let updated = match (existing.find(BEGIN), existing.find(END)) {
        (Some(start), Some(end)) if end > start => {
            let mut text = String::with_capacity(existing.len());
            text.push_str(&existing[..start]);
            text.push_str(&block(&plan.line));
            text.push_str(existing[end + END.len()..].trim_start_matches('\n'));
            text
        }
        _ => {
            let mut text = existing;
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&block(&plan.line));
            text
        }
    };
    if let Some(parent) = plan.file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write beside the target and rename, so an interrupted run cannot leave a
    // half-written startup file behind.
    let temp = plan.file.with_extension("tadoru-tmp");
    std::fs::write(&temp, with_bom(&plan.file, updated))?;
    std::fs::rename(&temp, &plan.file)?;
    Ok(())
}

/// Adds a byte order mark to a PowerShell script.
///
/// Without one, Windows PowerShell 5.1 reads a `.ps1` as the system code page
/// rather than UTF-8, so a path with Japanese in it never resolves. Other
/// shells read their startup files as UTF-8 and would show the mark as text.
fn with_bom(file: &Path, text: String) -> Vec<u8> {
    let powershell = file
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ps1"));
    if !powershell || text.starts_with('\u{feff}') {
        return text.into_bytes();
    }
    let mut bytes = Vec::with_capacity(text.len() + 3);
    bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    bytes.extend_from_slice(text.as_bytes());
    bytes
}

/// Asks before touching a file this program does not own.
pub fn confirm() -> Result<bool, Error> {
    if !std::io::stdin().is_terminal() {
        return Err(
            "not running from a terminal, so nothing was written. Re-run with --yes to write it anyway"
                .into(),
        );
    }
    eprint!("Write this? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn home() -> Result<PathBuf, Error> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .ok_or_else(|| "cannot locate the home directory".into())
}

/// The profile PowerShell 7 loads for the current user.
fn powershell_profile() -> Result<PathBuf, Error> {
    let documents = directories::UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
        .unwrap_or(home()?.join("Documents"));
    Ok(documents
        .join("PowerShell")
        .join("Microsoft.PowerShell_profile.ps1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_in(file: PathBuf, line: &str) -> Plan {
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        let found = extract(&existing);
        Plan {
            shell: Shell::Bash,
            file,
            current: found.as_deref() == Some(line),
            replacing: found.is_some(),
            taken: Vec::new(),
            line: line.to_string(),
        }
    }

    #[test]
    fn a_powershell_profile_is_written_so_5_1_can_read_a_path_with_japanese() {
        let root = crate::testing::temp_dir().join(format!("tadoru-enc-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("Microsoft.PowerShell_profile.ps1");

        let mut plan = plan_in(file.clone(), "line");
        plan.shell = Shell::Powershell;
        apply(&plan).unwrap();

        // Windows PowerShell 5.1 reads a .ps1 without this mark as the system
        // code page, and a path with Japanese in it then resolves to nothing.
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "no byte order mark");

        // Rewriting must not stack a second mark inside the file.
        let mut again = plan_in(file.clone(), "other");
        again.shell = Shell::Powershell;
        apply(&again).unwrap();
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(bytes.iter().filter(|&&b| b == 0xEF).count(), 1);

        // A bash rc file is read as UTF-8 and would show the mark as text.
        let rc = root.join("rc");
        apply(&plan_in(rc.clone(), "line")).unwrap();
        assert_ne!(&std::fs::read(&rc).unwrap()[..3], &[0xEF, 0xBB, 0xBF]);

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn the_powershell_line_reads_the_reply_as_utf8() {
        let plan = plan_for(
            Shell::Powershell,
            Path::new(r"C:\日本語\tadoru.exe"),
            &Names::default(),
        )
        .unwrap();
        // Without this the console code page decodes the generated function,
        // and the path inside it stops pointing at the executable.
        assert!(
            plan.line
                .contains("[Console]::OutputEncoding = [System.Text.Encoding]::UTF8"),
            "{}",
            plan.line
        );
        // Whatever the shell had is put back, so the block leaves no trace.
        assert!(
            plan.line
                .contains("[Console]::OutputEncoding = $tadoruEncoding")
        );
        assert!(plan.line.contains("Remove-Variable tadoruEncoding"));
    }

    #[test]
    fn a_startup_file_that_is_not_utf8_stops_the_run() {
        let root = crate::testing::temp_dir().join(format!("tadoru-bad-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("rc");
        // Shift_JIS for a Japanese word: valid on the reader's disk, not UTF-8.
        std::fs::write(&file, [0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, 0x0A]).unwrap();

        // Reading it as empty would replace the whole file with our block.
        let error = read_existing(&file).unwrap_err().to_string();
        assert!(error.contains("by hand"), "{error}");
        assert_eq!(std::fs::read(&file).unwrap().len(), 7);

        crate::testing::remove_tree(&root);
    }

    #[test]
    fn writing_twice_leaves_one_block_and_keeps_what_was_there() {
        let root = crate::testing::temp_dir().join(format!("tadoru-setup-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("rc");
        std::fs::write(&file, "eval \"$(zoxide init bash)\"\n").unwrap();

        apply(&plan_in(file.clone(), "line one")).unwrap();
        let once = std::fs::read_to_string(&file).unwrap();
        assert!(once.starts_with("eval \"$(zoxide init bash)\""), "{once}");
        assert_eq!(once.matches(BEGIN).count(), 1);

        // A second run replaces the block instead of stacking another copy,
        // which is what makes re-running after an update safe.
        apply(&plan_in(file.clone(), "line two")).unwrap();
        let twice = std::fs::read_to_string(&file).unwrap();
        assert_eq!(twice.matches(BEGIN).count(), 1, "{twice}");
        assert!(twice.contains("line two"), "{twice}");
        assert!(!twice.contains("line one"), "{twice}");
        assert!(twice.contains("zoxide init bash"), "the file was clobbered");

        // Recognising its own block is what lets a run report "nothing to do".
        assert!(plan_in(file.clone(), "line two").is_noop());
        assert!(!plan_in(file.clone(), "line three").is_noop());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_line_goes_after_whatever_the_file_already_loads() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-setup-order-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("rc");
        std::fs::write(&file, "eval \"$(zoxide init bash)\"\n").unwrap();
        apply(&plan_in(file.clone(), "eval \"$(tadoru init bash)\"")).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        // zoxide defines z and zi as aliases and an alias beats a function, so
        // tadoru has to be read afterwards to take them over.
        assert!(
            text.find("zoxide").unwrap() < text.find("tadoru").unwrap(),
            "{text}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cmd_is_told_where_its_files_go_instead() {
        let error = plan_for(Shell::Cmd, Path::new("tadoru.exe"), &Names::default()).unwrap_err();
        assert!(error.to_string().contains("init cmd --out"), "{error}");
    }

    #[test]
    fn a_chosen_name_is_carried_into_the_startup_line() {
        // The block calls init again on every shell start, so the names have
        // to travel with it or a new shell would come back as c.
        let names = Names::new(Some("j".into()), true).unwrap();
        let plan = plan_for(Shell::Bash, Path::new("/opt/tadoru"), &names).unwrap();
        assert!(
            plan.line.ends_with("init bash --cmd j --no-z)\""),
            "{}",
            plan.line
        );
        let plan = plan_for(Shell::Bash, Path::new("/opt/tadoru"), &Names::default()).unwrap();
        assert!(plan.line.ends_with("init bash)\""), "{}", plan.line);
    }
}
