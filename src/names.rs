//! What the shell commands are called, and whether those names are already
//! taken by something else on PATH.
//!
//! The short names are the point of the tool, so they stay the default. But a
//! single letter is exactly the kind of name another script already has, and
//! `cf` is the Cloud Foundry command line tool. zoxide faces the same problem
//! with `z` and `zi` and lets `init` choose the names; this follows it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

type Error = Box<dyn std::error::Error>;

/// Told to anyone whose names turned out to be taken.
pub const RENAME_HINT: &str =
    "Pick another name with --cmd, or keep zoxide's own z and zi with --no-z.";

/// What a generated command does.
///
/// The cmd.exe scripts key their shortcuts off this rather than off the name,
/// so a directory picker renamed from `c` still takes `-` for the directory
/// it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Pick a directory under the current one, or `-` for the previous one.
    Dirs,
    /// Pick a file and go to the folder holding it.
    Files,
    /// Pick from the zoxide history.
    Recent,
    /// Go straight to the best zoxide match, or home with no arguments.
    Jump,
}

impl Role {
    /// The picker mode behind the command. `Jump` asks zoxide directly.
    pub fn mode(self) -> Option<&'static str> {
        match self {
            Role::Dirs => Some("dirs"),
            Role::Files => Some("files"),
            Role::Recent => Some("recent"),
            Role::Jump => None,
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Role::Dirs => "pick a directory under the current one and cd into it",
            Role::Files => "pick a file and cd into its directory",
            Role::Recent => "pick a directory from the zoxide history",
            Role::Jump => "jump to a directory from zoxide",
        }
    }
}

/// The names the shell integration defines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Names {
    /// The directory picker. The file picker is this with `f` added.
    base: String,
    /// Whether `z` and `zi` are defined. zoxide defines them itself, and
    /// someone who wants zoxide's own versions keeps them by turning this off.
    zoxide: bool,
}

impl Default for Names {
    fn default() -> Self {
        Self {
            base: "c".to_string(),
            zoxide: true,
        }
    }
}

impl Names {
    pub fn new(base: Option<String>, no_z: bool) -> Result<Self, Error> {
        let base = base.unwrap_or_else(|| "c".to_string());
        // Letters, digits and underscores, starting with a letter, are the
        // names every one of PowerShell, bash and cmd.exe file names accept.
        let mut chars = base.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            return Err(format!(
                "--cmd {base:?}: use letters, digits and underscores, starting with a letter"
            )
            .into());
        }
        if !no_z && (base == "z" || base == "zi") {
            return Err(format!(
                "--cmd {base} would collide with the z and zi this also defines; add --no-z or pick another name"
            )
            .into());
        }
        Ok(Self {
            base,
            zoxide: !no_z,
        })
    }

    /// Whether `z` and `zi` are part of the integration.
    pub fn zoxide(&self) -> bool {
        self.zoxide
    }

    /// Every command, with what it does, in the order they are written out.
    pub fn commands(&self) -> Vec<(String, Role)> {
        let mut commands = vec![
            (self.base.clone(), Role::Dirs),
            (format!("{}f", self.base), Role::Files),
        ];
        if self.zoxide {
            commands.push(("zi".to_string(), Role::Recent));
            commands.push(("z".to_string(), Role::Jump));
        }
        commands
    }

    /// The names as a short list for a comment, such as `c / cf / zi / z`.
    pub fn listing(&self) -> String {
        self.commands()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>()
            .join(" / ")
    }

    /// The flags that give these names again, for a line that calls `init`.
    /// Empty for the defaults, so an unchanged setup writes the same line.
    pub fn flags(&self) -> String {
        let mut flags = String::new();
        if self.base != "c" {
            flags.push_str(" --cmd ");
            flags.push_str(&self.base);
        }
        if !self.zoxide {
            flags.push_str(" --no-z");
        }
        flags
    }
}

/// Another program on PATH that answers to one of the names.
#[derive(Debug, PartialEq, Eq)]
pub struct Taken {
    pub name: String,
    pub path: PathBuf,
    /// Whether it sits in a folder earlier on PATH than the generated
    /// scripts, and so is found first. `None` when their folder is not on
    /// PATH at all.
    pub first: Option<bool>,
}

impl Taken {
    /// One line for scripts written into `dir` with the extension `ext`.
    pub fn for_scripts(&self, dir: &Path, ext: &str) -> String {
        let ours = dir.join(format!("{}{ext}", self.name));
        match self.first {
            Some(true) => format!(
                "  {}: {} comes earlier on PATH, so it is found before {}",
                self.name,
                self.path.display(),
                ours.display()
            ),
            Some(false) => format!(
                "  {}: {} is also on PATH; {} comes earlier and hides it",
                self.name,
                self.path.display(),
                ours.display()
            ),
            None => format!(
                "  {}: {} is on PATH and {} is not, so {} still finds that one",
                self.name,
                self.path.display(),
                dir.display(),
                self.name
            ),
        }
    }

    /// One line for a name defined as a shell function.
    pub fn for_functions(&self) -> String {
        format!("    {:<3} {}", self.name, self.path.display())
    }
}

/// The programs on PATH that share a name with the integration.
///
/// Folders in `skip` are not looked in: they hold tadoru's own scripts, which
/// are being replaced rather than collided with. `scripts` is where the new
/// scripts live, used to say which of the two is found first.
pub fn taken(names: &Names, skip: &[PathBuf], scripts: Option<&Path>) -> Vec<Taken> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let pathext = std::env::var_os("PATHEXT");
    taken_in(names, skip, scripts, &path, pathext.as_deref())
}

fn taken_in(
    names: &Names,
    skip: &[PathBuf],
    scripts: Option<&Path>,
    path: &OsStr,
    pathext: Option<&OsStr>,
) -> Vec<Taken> {
    let dirs: Vec<PathBuf> = std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    let ours = scripts.and_then(|scripts| dirs.iter().position(|dir| same_dir(dir, scripts)));
    let mut found = Vec::new();
    for (name, _) in names.commands() {
        for (index, dir) in dirs.iter().enumerate() {
            if skip.iter().any(|skip| same_dir(dir, skip)) {
                continue;
            }
            if let Some(file) = executable(dir, &name, pathext) {
                found.push(Taken {
                    name,
                    path: file,
                    first: ours.map(|ours| index < ours),
                });
                break;
            }
        }
    }
    found
}

#[cfg(windows)]
fn executable(dir: &Path, name: &str, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let mut extensions: Vec<String> = pathext
        .map(|pathext| {
            pathext
                .to_string_lossy()
                .split(';')
                .filter(|ext| !ext.is_empty())
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_else(|| [".com", ".exe", ".bat", ".cmd"].map(String::from).to_vec());
    // PowerShell also runs a script it finds on PATH.
    if !extensions.iter().any(|ext| ext == ".ps1") {
        extensions.push(".ps1".to_string());
    }
    extensions
        .into_iter()
        .map(|ext| dir.join(format!("{name}{ext}")))
        .find(|file| file.is_file())
}

#[cfg(not(windows))]
fn executable(dir: &Path, name: &str, _pathext: Option<&OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let file = dir.join(name);
    let meta = std::fs::metadata(&file).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(file)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return a == b;
    }
    let normal = |path: &Path| {
        let text = path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        if cfg!(windows) {
            text.to_lowercase()
        } else {
            text
        }
    };
    normal(a) == normal(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_must_suit_every_shell_and_stay_clear_of_z() {
        assert_eq!(Names::new(None, false).unwrap(), Names::default());
        assert_eq!(
            Names::new(Some("j".into()), false).unwrap().listing(),
            "j / jf / zi / z"
        );
        assert_eq!(
            Names::new(Some("go_to".into()), true).unwrap().listing(),
            "go_to / go_tof"
        );
        for bad in ["", "1c", "c d", "c-d", "日本", "c;rm"] {
            assert!(Names::new(Some(bad.into()), false).is_err(), "{bad:?}");
        }
        // z and zi are already the zoxide pair, so they are free only once
        // that pair is not being defined.
        assert!(Names::new(Some("z".into()), false).is_err());
        assert!(Names::new(Some("zi".into()), false).is_err());
        assert!(Names::new(Some("z".into()), true).is_ok());
    }

    #[test]
    fn the_flags_give_back_the_same_names_and_nothing_for_the_defaults() {
        // An unchanged setup must write an unchanged line, or re-running it
        // would keep replacing a block that says the same thing.
        assert_eq!(Names::default().flags(), "");
        assert_eq!(
            Names::new(Some("j".into()), true).unwrap().flags(),
            " --cmd j --no-z"
        );
        assert_eq!(Names::new(None, true).unwrap().flags(), " --no-z");
    }

    #[test]
    fn a_name_already_on_path_is_found_with_which_one_comes_first() {
        let root = crate::testing::temp_dir().join(format!("tadoru-names-{}", std::process::id()));
        let other = root.join("other");
        let ours = root.join("ours");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&ours).unwrap();
        #[cfg(windows)]
        let taken_file = {
            let file = other.join("c.bat");
            std::fs::write(&file, "@echo off\r\n").unwrap();
            file
        };
        #[cfg(not(windows))]
        let taken_file = {
            use std::os::unix::fs::PermissionsExt;
            let file = other.join("c");
            std::fs::write(&file, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            file
        };
        let names = Names::default();
        let pathext = Some(OsStr::new(".COM;.EXE;.BAT;.CMD"));
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();

        // Earlier on PATH than the new scripts: that one answers to c.
        let found = taken_in(&names, &[], Some(&ours), &path(&[&other, &ours]), pathext);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "c");
        assert!(same_dir(found[0].path.parent().unwrap(), &other));
        assert_eq!(found[0].path.file_name(), taken_file.file_name());
        assert_eq!(found[0].first, Some(true));

        // Later: the new scripts win and hide it.
        let found = taken_in(&names, &[], Some(&ours), &path(&[&ours, &other]), pathext);
        assert_eq!(found[0].first, Some(false));

        // The scripts' own folder not on PATH yet: the other one still runs.
        let found = taken_in(&names, &[], Some(&ours), &path(&[&other]), pathext);
        assert_eq!(found[0].first, None);

        // tadoru's own scripts are being replaced, not collided with.
        let found = taken_in(
            &names,
            std::slice::from_ref(&other),
            Some(&ours),
            &path(&[&other]),
            pathext,
        );
        assert!(found.is_empty(), "{found:?}");

        crate::testing::remove_tree(&root);
    }
}
