//! Actions are loaded only from the user's config directory, never the selected project.
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::OnceLock;

use serde::Deserialize;

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    #[default]
    Terminal,
    Detach,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Target {
    #[default]
    Any,
    File,
    Directory,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub name: String,
    program: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    target: Target,
    #[serde(default)]
    pub run: RunMode,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: u32,
    #[serde(default = "yes")]
    include_defaults: bool,
    actions: Vec<Definition>,
}
fn yes() -> bool {
    true
}

#[derive(Clone)]
pub enum Action {
    Reveal,
    Open,
    TempCopy,
    TempFolder,
    Copy,
    Editor,
    Custom {
        definition: Definition,
        config_dir: PathBuf,
    },
}

impl Action {
    pub fn name(&self) -> &str {
        match self {
            Self::Reveal => "Open in file manager",
            Self::Open => "Open with default application",
            Self::TempCopy => "Open temporary copy (TEMP_)",
            Self::TempFolder => "Open temporary copies folder",
            Self::Copy => "Copy path",
            Self::Editor => "Open in VS Code",
            Self::Custom { definition, .. } => &definition.name,
        }
    }

    /// The character that runs this action, if it has one.
    ///
    /// The menu is read, not typed at, so every letter comes from a word in
    /// its own name and stays put as the list grows. Four names start with
    /// Open, so `o` goes to the temporary copy, which has no other way in,
    /// and the plain one takes `d` for default.
    pub fn key(&self) -> Option<char> {
        match self {
            Self::Reveal => Some('f'),
            Self::Open => Some('d'),
            Self::TempCopy => Some('o'),
            Self::TempFolder => Some('t'),
            Self::Copy => Some('c'),
            Self::Editor => Some('v'),
            Self::Custom { definition, .. } => definition.key.as_deref().and_then(parse_key),
        }
    }

    pub fn run_mode(&self) -> RunMode {
        match self {
            Self::Custom { definition, .. } => definition.run,
            _ => RunMode::Detach,
        }
    }

    pub fn prepare(&self, target: &Path) -> Result<Command> {
        let Self::Custom {
            definition,
            config_dir,
        } = self
        else {
            return Err("not a custom action".into());
        };
        let context = Context::new(target, config_dir)?;
        if !definition.matches(target) {
            return Err("this action does not apply to the selected item".into());
        }
        let program = resolve(&definition.program, config_dir)?;
        let powershell = program.file_stem().is_some_and(|name| {
            name.eq_ignore_ascii_case("pwsh") || name.eq_ignore_ascii_case("powershell")
        });
        let mut command = if program
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("ps1"))
        {
            let mut command = Command::new(resolve("pwsh", config_dir)?);
            command.args(["-NoProfile", "-File"]).arg(&program);
            command
        } else {
            Command::new(program)
        };
        let mut script_argument = false;
        for arg in &definition.args {
            let value = expand(arg, &context)?;
            // PowerShell -File is explicitly a script path, unlike arbitrary arguments.
            let value = if script_argument && Path::new(&value).is_relative() {
                config_dir.join(value).into_os_string()
            } else {
                value
            };
            command.arg(value);
            script_argument = powershell && arg.eq_ignore_ascii_case("-file");
        }
        let cwd = expand(definition.cwd.as_deref().unwrap_or("{dir}"), &context)?;
        let cwd = if Path::new(&cwd).is_absolute() {
            PathBuf::from(cwd)
        } else {
            config_dir.join(cwd)
        };
        if !cwd.is_dir() {
            return Err(format!(
                "working directory does not exist: {}",
                cwd.display()
            ));
        }
        command
            .current_dir(cwd)
            .env("TADORU_TARGET", &context.path)
            .env("TADORU_DIR", &context.dir)
            .env("TADORU_CONFIG", &context.config);
        Ok(command)
    }

    pub fn execute_detached(&self, target: &Path) -> Result<Option<PathBuf>> {
        if matches!(self, Self::TempCopy) {
            let config = crate::config::Config::load().map_err(|error| error.to_string())?;
            let limit = config
                .temp_copy_max_mib
                .checked_mul(1024 * 1024)
                .ok_or("temp_copy_max_mib is too large")?;
            let root = temporary_copies_folder(&std::env::temp_dir())?;
            let copy = temporary_copy(target, &root, limit)?;
            crate::open::launch(&copy).map_err(|error| {
                format!("Copy saved at {}; cannot open: {error}", copy.display())
            })?;
            return Ok(Some(copy));
        }
        match self {
            Self::TempCopy => unreachable!(),
            Self::TempFolder => {
                let root = temporary_copies_folder(&std::env::temp_dir())?;
                crate::open::reveal(&root).map_err(|error| error.to_string())
            }
            Self::Reveal => crate::open::reveal(target).map_err(|error| error.to_string()),
            Self::Open => crate::open::launch(target).map_err(|error| error.to_string()),
            Self::Copy => copy_path(target),
            Self::Editor => {
                let mut command = Command::new(resolve("code", &config_dir()?)?);
                command.arg(target);
                spawn_detached(command)
            }
            Self::Custom { .. } => spawn_detached(self.prepare(target)?),
        }
        .map(|()| None)
    }

    pub fn execute_terminal(&self, target: &Path) -> Result<ExitStatus> {
        // The foreground child receives Ctrl-C normally; the picker survives to restore its UI.
        static HANDLER: OnceLock<Result<()>> = OnceLock::new();
        HANDLER
            .get_or_init(|| ctrlc::set_handler(|| {}).map_err(|error| error.to_string()))
            .clone()?;
        let mut command = self.prepare(target)?;
        // stdout is reserved for the final cd destination, even while the UI is suspended.
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::from(io::stderr()))
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| error.to_string())
    }
}

/// Create an independent, writable copy. Never reuse or remove an existing directory.
fn temporary_copies_folder(temp_root: &Path) -> Result<PathBuf> {
    let root = temp_root.join("tadoru-copies");
    fs::create_dir_all(&root)
        .map_err(|error| format!("Cannot create {}: {error}", root.display()))?;
    Ok(root)
}

fn temporary_copy(source: &Path, temp_root: &Path, limit: u64) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    if limit == 0 {
        return Err("Temporary copies are disabled (temp_copy_max_mib = 0)".into());
    }
    let mut input = fs::File::open(source).map_err(|error| error.to_string())?;
    let metadata = input.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() {
        return Err("Temporary copy is available for files only".into());
    }
    if metadata.len() > limit {
        return Err(format!(
            "Temporary copy blocked: {} bytes exceeds the {} byte limit (temp_copy_max_mib)",
            metadata.len(),
            limit
        ));
    }
    let mut name = OsString::from("TEMP_");
    name.push(source.file_name().ok_or("Missing filename")?);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let dir = loop {
        let dir = temp_root.join(format!(
            "tadoru-copy-{}-{stamp}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        match builder.create(&dir) {
            Ok(()) => break dir,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.to_string()),
        }
    };
    let copy = dir.join(name);
    let result = (|| -> io::Result<()> {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&copy)?;
        copy_bounded(&mut input, &mut output, limit)?;
        output.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&copy);
        let _ = fs::remove_dir(&dir);
        return Err(format!("Cannot create temporary copy: {error}"));
    }
    Ok(copy)
}

fn copy_bounded(input: &mut impl Read, output: &mut impl Write, limit: u64) -> io::Result<()> {
    io::copy(&mut input.take(limit), output)?;
    // Check for growth after the metadata check without writing past the limit.
    if input.read(&mut [0u8; 1])? != 0 {
        return Err(io::Error::other(
            "Temporary copy exceeded its size limit; copy cancelled",
        ));
    }
    Ok(())
}

impl Definition {
    fn matches(&self, path: &Path) -> bool {
        match self.target {
            Target::Any => true,
            Target::File => path.is_file(),
            Target::Directory => path.is_dir(),
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    crate::config::Config::path()
        .map(|path| path.with_file_name("actions.json"))
        .ok_or_else(|| "cannot locate the user configuration directory".into())
}
fn config_dir() -> Result<PathBuf> {
    Ok(config_path()?
        .parent()
        .expect("config file has parent")
        .to_path_buf())
}

pub fn init() -> Result<PathBuf> {
    let path = config_path()?;
    fs::create_dir_all(path.parent().expect("config parent")).map_err(|error| error.to_string())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            format!(
                "{}: {error} (existing files are never overwritten)",
                path.display()
            )
        })?;
    file.write_all(include_bytes!("../examples/config/actions.json"))
        .map_err(|error| error.to_string())?;
    Ok(path)
}

fn defaults(target: &Path) -> Vec<Action> {
    let mut actions = vec![Action::Reveal, Action::Editor, Action::Copy];
    if target.is_file() {
        actions.push(Action::Open);
        actions.push(Action::TempCopy);
    }
    actions
}

/// Commands that ignore the selection. Listing them beside the actions on the
/// selected item invites reading them as one, so they are kept apart.
pub fn tools() -> Vec<Action> {
    vec![Action::TempFolder]
}

/// Invalid user config leaves the built-in menu available with an explanation.
pub fn load(target: &Path) -> (Vec<Action>, Option<String>) {
    match config_path().and_then(|file| read(&file, target)) {
        Ok(actions) => (actions, None),
        Err(error) => (defaults(target), Some(error)),
    }
}

pub fn check() -> Result<PathBuf> {
    let path = config_path()?;
    let text = fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(path)
}

fn read(file: &Path, target: &Path) -> Result<Vec<Action>> {
    let text = match fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(defaults(target)),
        Err(error) => return Err(format!("{}: {error}", file.display())),
    };
    let config = parse(&text).map_err(|error| format!("{}: {error}", file.display()))?;
    let mut actions = if config.include_defaults {
        defaults(target)
    } else {
        Vec::new()
    };
    for definition in config.actions {
        if definition.matches(target) {
            actions.push(Action::Custom {
                definition,
                config_dir: file.parent().expect("config parent").to_path_buf(),
            });
        }
    }
    Ok(actions)
}

/// One letter or digit, which the menu listens for on its own. Anything
/// longer is rejected so a typo is not silently dropped, and `/` is refused
/// because it opens the filter.
fn parse_key(value: &str) -> Option<char> {
    let mut chars = value.chars();
    let ch = chars.next()?.to_ascii_lowercase();
    (chars.next().is_none() && ch.is_ascii_alphanumeric()).then_some(ch)
}

fn parse(text: &str) -> Result<File> {
    let config: File = serde_json::from_str(text).map_err(|error| error.to_string())?;
    if config.version != 1 {
        return Err(format!("unsupported actions version: {}", config.version));
    }
    let dummy = Context {
        path: "target".into(),
        dir: "dir".into(),
        config: "config".into(),
    };
    let mut claimed: Vec<char> = Vec::new();
    for action in &config.actions {
        if action.name.trim().is_empty() || action.name.chars().any(char::is_control) {
            return Err("action name must be nonempty and on one line".into());
        }
        if let Some(key) = &action.key {
            let Some(ch) = parse_key(key) else {
                return Err(format!(
                    "{}: key must be one letter or digit, such as g",
                    action.name
                ));
            };
            if claimed.contains(&ch) {
                return Err(format!("{}: the key {ch} is already used", action.name));
            }
            claimed.push(ch);
        }
        if action.program.trim().is_empty() || action.program.contains('\0') {
            return Err(format!("{}: program is empty or invalid", action.name));
        }
        if ["{path}", "{dir}", "{config}"]
            .iter()
            .any(|token| action.program.contains(token))
        {
            return Err(format!(
                "{}: program must be a fixed executable or script path",
                action.name
            ));
        }
        for value in action.args.iter().chain(action.cwd.iter()) {
            if value.contains('\0') {
                return Err(format!("{}: NUL is not allowed", action.name));
            }
            expand(value, &dummy).map_err(|error| format!("{}: {error}", action.name))?;
        }
        // Do not substitute filenames into shell source code. Scripts can read TADORU_TARGET.
        let program_name = action
            .program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let shell = [
            "cmd",
            "cmd.exe",
            "pwsh",
            "pwsh.exe",
            "powershell",
            "powershell.exe",
            "sh",
            "bash",
            "zsh",
            "fish",
        ]
        .contains(&program_name.as_str());
        let shell_code = shell
            && action.args.iter().any(|arg| {
                ["-c", "/c", "/k", "-command", "-encodedcommand"]
                    .iter()
                    .any(|flag| arg.eq_ignore_ascii_case(flag))
            });
        if shell_code
            && action
                .args
                .iter()
                .any(|arg| arg.contains("{path}") || arg.contains("{dir}"))
        {
            return Err(format!(
                "{}: use a script file or TADORU_TARGET instead of substituting paths into shell code",
                action.name
            ));
        }
    }
    Ok(config)
}

struct Context {
    path: PathBuf,
    dir: PathBuf,
    config: PathBuf,
}
impl Context {
    fn new(target: &Path, config: &Path) -> Result<Self> {
        let path = std::path::absolute(target).map_err(|error| error.to_string())?;
        let metadata =
            fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let dir = if metadata.is_dir() {
            path.clone()
        } else {
            path.parent().ok_or("target has no parent")?.to_path_buf()
        };
        Ok(Self {
            path,
            dir,
            config: config.to_path_buf(),
        })
    }
}

/// Single-pass expansion preserves braces and shell metacharacters in the selected path.
fn expand(template: &str, context: &Context) -> Result<OsString> {
    let mut output = OsString::new();
    let mut remaining = template;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix("{{") {
            output.push("{");
            remaining = rest;
        } else if let Some(rest) = remaining.strip_prefix("}}") {
            output.push("}");
            remaining = rest;
        } else if let Some(rest) = remaining.strip_prefix('{') {
            let end = rest.find('}').ok_or("unclosed placeholder")?;
            output.push(match &rest[..end] {
                "path" => context.path.as_os_str(),
                "dir" => context.dir.as_os_str(),
                "config" => context.config.as_os_str(),
                other => return Err(format!("unknown placeholder {{{other}}}")),
            });
            remaining = &rest[end + 1..];
        } else {
            let ch = remaining.chars().next().expect("nonempty");
            output.push(ch.to_string());
            remaining = &remaining[ch.len_utf8()..];
        }
    }
    Ok(output)
}

fn resolve(program: &str, base: &Path) -> Result<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute()
        || program.contains('/')
        || program.contains('\\')
        || base.join(path).is_file()
    {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        };
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!("program not found: {}", path.display()))
        };
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        let candidate = directory.join(program);
        #[cfg(windows)]
        if path.extension().is_none() {
            for extension in ["exe", "com", "cmd", "bat"] {
                let candidate = candidate.with_extension(extension);
                if candidate.is_file() {
                    return std::path::absolute(candidate).map_err(|error| error.to_string());
                }
            }
        }
        if candidate.is_file() {
            return std::path::absolute(candidate).map_err(|error| error.to_string());
        }
    }
    Err(format!("program not found on PATH: {program}"))
}

fn spawn_detached(mut command: Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn copy_path(target: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(resolve("powershell", &config_dir()?)?);
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference = 'Stop'; Set-Clipboard -Value $env:TADORU_TARGET",
            ])
            .env("TADORU_TARGET", target)
            .stdin(Stdio::null())
            .creation_flags(0x08000000);
        let output = command.output().map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
            ("pbcopy", &[])
        } else if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            ("wl-copy", &[])
        } else {
            ("xclip", &["-selection", "clipboard"])
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("{program}: {error}"))?;
        let written = child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(target.as_os_str().as_encoded_bytes());
        let status = child.wait().map_err(|error| error.to_string())?;
        written.map_err(|error| error.to_string())?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{program}: {status}"))
        }
    }
}

#[cfg(test)]
pub fn test_definition(name: &str, key: Option<&str>) -> Definition {
    Definition {
        name: name.into(),
        program: "true".into(),
        args: Vec::new(),
        target: Target::Any,
        run: RunMode::Detach,
        cwd: None,
        key: key.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = crate::testing::temp_dir()
                .join(format!("tadoru-action-{}-{nonce}", std::process::id()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn action(&self, program: &str, args: Vec<String>) -> Action {
            Action::Custom {
                definition: Definition {
                    name: "test".into(),
                    program: program.into(),
                    args,
                    target: Target::Any,
                    run: RunMode::Terminal,
                    cwd: None,
                    key: None,
                },
                config_dir: self.0.clone(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(crate::testing::temp_dir().as_path()));
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn temporary_folder_is_shared_and_preserves_existing_copies() {
        let fixture = Fixture::new();
        let root = temporary_copies_folder(&fixture.0).unwrap();
        let existing = root.join("keep.txt");
        fs::write(&existing, "keep").unwrap();
        assert_eq!(temporary_copies_folder(&fixture.0).unwrap(), root);
        assert_eq!(fs::read_to_string(existing).unwrap(), "keep");
        // Reaching the folder is a command on tadoru, not on the selection,
        // so it is offered apart from the actions for the selected item.
        assert!(
            !defaults(&fixture.0)
                .iter()
                .any(|a| matches!(a, Action::TempFolder))
        );
        assert!(tools().iter().any(|a| matches!(a, Action::TempFolder)));
    }

    #[test]
    fn temporary_copy_limit_rejects_before_creating_files_and_accepts_boundary() {
        let fixture = Fixture::new();
        let source = fixture.0.join("large.bin");
        fs::write(&source, b"12345").unwrap();
        assert!(
            temporary_copy(&source, &fixture.0, 4)
                .unwrap_err()
                .contains("exceeds")
        );
        assert!(
            temporary_copy(&source, &fixture.0, 0)
                .unwrap_err()
                .contains("disabled")
        );
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        let copy = temporary_copy(&source, &fixture.0, 5).unwrap();
        assert_eq!(fs::read(copy).unwrap(), b"12345");
    }

    #[test]
    fn bounded_copy_never_writes_beyond_limit_even_if_source_is_larger() {
        let mut output = Vec::new();
        assert!(copy_bounded(&mut &b"123456"[..], &mut output, 5).is_err());
        assert_eq!(output, b"12345");
        output.clear();
        copy_bounded(&mut &b"12345"[..], &mut output, 5).unwrap();
        assert_eq!(output, b"12345");
    }

    #[test]
    fn temporary_copies_are_unique_and_do_not_modify_the_original() {
        let fixture = Fixture::new();
        let source = fixture.0.join("原本 file.xlsx");
        fs::write(&source, b"original contents").unwrap();
        let first = temporary_copy(&source, &fixture.0, 100).unwrap();
        let second = temporary_copy(&source, &fixture.0, 100).unwrap();
        assert_ne!(first.parent(), second.parent());
        assert_eq!(first.file_name().unwrap(), "TEMP_原本 file.xlsx");
        assert_eq!(fs::read(&first).unwrap(), b"original contents");
        fs::write(&first, b"edited copy").unwrap();
        assert_eq!(fs::read(&source).unwrap(), b"original contents");
        assert_eq!(fs::read(&second).unwrap(), b"original contents");
        assert!(temporary_copy(&fixture.0, &fixture.0, 100).is_err());
        assert!(
            defaults(&source)
                .iter()
                .any(|action| matches!(action, Action::TempCopy))
        );
        assert!(
            !defaults(&fixture.0)
                .iter()
                .any(|action| matches!(action, Action::TempCopy))
        );
    }

    #[test]
    fn validates_version_fields_and_templates_without_executing_commands() {
        assert!(parse(include_str!("../examples/config/actions.json")).is_ok());
        for text in [
            r#"{"version":2,"actions":[]}"#,
            r#"{"version":1,"action":[]}"#,
            r#"{"version":1,"actions":[{"name":"a","program":"tool","run":"oops"}]}"#,
            r#"{"version":1,"actions":[{"name":"a","program":"tool","args":["{unknown}"]}]}"#,
            r#"{"version":1,"actions":[{"name":"a","program":"cmd.exe","args":["/c","echo {path}"]}]}"#,
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn expansion_is_single_pass_and_preserves_argument_boundaries() {
        let context = Context {
            path: PathBuf::from("日本語 {dir} & % ! $() space"),
            dir: "/parent".into(),
            config: "/config".into(),
        };
        assert_eq!(
            expand("--target={path}", &context).unwrap(),
            OsString::from("--target=日本語 {dir} & % ! $() space")
        );
        assert_eq!(
            expand("{{literal}} {dir}", &context).unwrap(),
            OsString::from("{literal} /parent")
        );
    }

    #[test]
    fn a_key_must_be_one_character_and_may_not_repeat() {
        let one = |key: &str| {
            format!(r#"{{"version":1,"actions":[{{"name":"a","program":"p","key":"{key}"}}]}}"#)
        };
        assert_eq!(parse_key("g"), Some('g'));
        assert_eq!(parse_key("G"), Some('g'));
        assert_eq!(parse_key("7"), Some('7'));
        // Silently ignoring these would leave a key that never fires.
        for bad in ["", "gg", "alt+g", "/", " "] {
            assert_eq!(parse_key(bad), None, "{bad:?}");
            assert!(
                parse(&one(bad)).err().unwrap().contains("one letter"),
                "{bad:?}"
            );
        }
        assert!(parse(&one("g")).is_ok());
        let twice = r#"{"version":1,"actions":[
            {"name":"a","program":"p","key":"g"},
            {"name":"b","program":"p","key":"G"}]}"#;
        assert!(parse(twice).err().unwrap().contains("already used"));
    }

    #[test]
    fn target_filtering_and_config_errors_do_not_run_anything() {
        let fixture = Fixture::new();
        let file = fixture.0.join("actions.json");
        fs::write(&file, r#"{"version":1,"include_defaults":false,"actions":[{"name":"folders","program":"missing","target":"directory"},{"name":"files","program":"missing","target":"file"}]}"#).unwrap();
        let target = fixture.0.join("file.txt");
        fs::write(&target, "").unwrap();
        assert_eq!(read(&file, &target).unwrap()[0].name(), "files");
        assert_eq!(read(&file, &fixture.0).unwrap()[0].name(), "folders");
        fs::write(&file, "invalid json").unwrap();
        assert!(read(&file, &target).err().unwrap().contains("actions.json"));
    }

    #[test]
    fn powershell_script_receives_literal_paths_and_runs_in_target_parent() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("scripts")).unwrap();
        let target = fixture.0.join("日本語 {dir} & % ! space.txt");
        fs::write(&target, "").unwrap();
        fs::write(fixture.0.join("scripts/inspect.ps1"), r#"param([string]$Target, [string]$Literal)
[pscustomobject]@{ target=$Target; literal=$Literal; cwd=(Get-Location).Path; environment=$env:TADORU_TARGET } | ConvertTo-Json -Compress
"#).unwrap();
        for (program, args) in [
            (
                "pwsh",
                vec![
                    "-NoProfile",
                    "-File",
                    "scripts/inspect.ps1",
                    "{path}",
                    "literal $() & % !",
                ],
            ),
            ("scripts/inspect.ps1", vec!["{path}", "literal $() & % !"]),
        ] {
            let action = fixture.action(program, args.into_iter().map(String::from).collect());
            let output = action.prepare(&target).unwrap().output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["target"], target.to_string_lossy().as_ref());
            assert_eq!(value["environment"], target.to_string_lossy().as_ref());
            assert_eq!(value["literal"], "literal $() & % !");
            assert_eq!(Path::new(value["cwd"].as_str().unwrap()), fixture.0);
        }
    }

    #[test]
    fn terminal_output_is_not_a_cd_destination() {
        const MARKER: &str = "tadoru-action-stdout-marker";
        if std::env::var_os("TADORU_ACTION_TEST_CHILD").is_some() {
            let fixture = Fixture::new();
            fs::write(
                fixture.0.join("output.ps1"),
                format!("Write-Output '{MARKER}'\nexit 9\n"),
            )
            .unwrap();
            let action = fixture.action("output.ps1", vec![]);
            assert_eq!(action.execute_terminal(&fixture.0).unwrap().code(), Some(9));
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "actions::tests::terminal_output_is_not_a_cd_destination",
                "--nocapture",
            ])
            .env("TADORU_ACTION_TEST_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(MARKER));
        assert!(String::from_utf8_lossy(&output.stderr).contains(MARKER));
    }

    #[cfg(windows)]
    #[test]
    fn batch_script_preserves_metacharacters_in_target_arguments() {
        let fixture = Fixture::new();
        let target = fixture
            .0
            .join("日本語 & %TADORU_TEST_PAYLOAD% ! ^ {dir}.txt");
        fs::write(&target, "").unwrap();
        fs::write(fixture.0.join("inspect.cmd"), "@echo off\r\nsetlocal DisableDelayedExpansion\r\nchcp 65001 >nul\r\nset \"captured=%~1\"\r\nset captured\r\nexit /b 9\r\n").unwrap();
        let action = fixture.action("inspect.cmd", vec!["{path}".into()]);
        let output = action
            .prepare(&target)
            .unwrap()
            .env("TADORU_TEST_PAYLOAD", "EXPANDED")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("captured={}", target.display())
        );
    }
}
