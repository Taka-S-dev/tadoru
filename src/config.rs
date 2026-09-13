use std::path::PathBuf;

use serde::Deserialize;

/// User configuration. Every field has a default so the file is optional.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Show Nerd Font icons. The terminal must use a compatible font.
    pub icons: bool,
    /// Capture mouse input for list selection and scrolling.
    pub mouse: bool,
    /// Maximum temporary copy size in MiB; zero disables temporary copies.
    pub temp_copy_max_mib: u64,
    /// Directory names to skip at any depth.
    pub exclude: Vec<String>,
    /// How many entries a single scan may collect; zero lifts the ceiling.
    ///
    /// Every path found is held in memory for the whole session, so a scan
    /// started near the top of a drive can cost hundreds of megabytes. The
    /// list says when it stopped early, rather than quietly going short.
    pub scan_limit: usize,
    /// Absolute paths never treated as project roots. Reserved for later use.
    #[allow(dead_code)]
    pub exclude_roots: Vec<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            icons: false,
            mouse: true,
            temp_copy_max_mib: 100,
            exclude: [".git", "node_modules", "dist", "build", "target"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            scan_limit: 200_000,
            exclude_roots: Vec::new(),
        }
    }
}

impl Config {
    /// Explicit override, portable `config/` beside the executable, then user config.
    pub fn path() -> Option<PathBuf> {
        // An explicit override also isolates subprocess tests on Windows, where
        // Known Folder APIs do not follow a replaced APPDATA environment variable.
        if let Some(dir) = std::env::var_os("TADORU_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
            return Some(std::path::absolute(dir).ok()?.join("config.toml"));
        }
        if let Some(path) = std::env::current_exe().ok().and_then(portable_config) {
            return Some(path);
        }
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("tadoru").join("config.toml"))
    }

    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("{}: {err}", path.display()).into()),
        };
        toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()).into())
    }
}

/// Writes a commented copy of the defaults for the reader to edit.
///
/// Nothing is written on a normal run: a tool that has not been configured
/// should leave no trace, and every setting already has a default. Asking for
/// the file is what creates it, which mirrors `actions init`.
pub fn init() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = Config::path().ok_or("cannot locate the user configuration directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            format!(
                "{}: {error} (existing files are never overwritten)",
                path.display()
            )
        })?;
    std::io::Write::write_all(&mut file, include_bytes!("../examples/config/config.toml"))?;
    Ok(path)
}

fn portable_config(exe: PathBuf) -> Option<PathBuf> {
    let dir = exe.parent()?.join("config");
    dir.is_dir().then(|| dir.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_written_file_parses_and_matches_the_defaults() {
        // A template that drifts from the defaults would quietly change the
        // tool's behaviour for anyone who runs config init.
        let text = include_str!("../examples/config/config.toml");
        let written: Config = toml::from_str(text).expect("the template must parse");
        let default = Config::default();
        assert_eq!(written.icons, default.icons);
        assert_eq!(written.mouse, default.mouse);
        assert_eq!(written.temp_copy_max_mib, default.temp_copy_max_mib);
        assert_eq!(written.exclude, default.exclude);
    }

    #[test]
    fn init_writes_once_and_refuses_to_overwrite() {
        let root = crate::testing::temp_dir().join(format!("tadoru-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // Safe to set here: the tests in this file run in one process and the
        // override is read on every call.
        unsafe { std::env::set_var("TADORU_CONFIG_DIR", &root) };

        let path = init().expect("the first run writes the file");
        assert_eq!(path, root.join("config.toml"));
        std::fs::write(
            &path,
            "icons = true
",
        )
        .unwrap();

        // Someone's edited settings must survive a second run.
        let error = init().expect_err("the second run must refuse");
        assert!(error.to_string().contains("never overwritten"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "icons = true
"
        );

        unsafe { std::env::remove_var("TADORU_CONFIG_DIR") };
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn portable_directory_enables_configuration_even_before_files_exist() {
        let root = std::env::temp_dir().join(format!("tadoru-portable-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let exe = root.join("tadoru.exe");
        assert_eq!(portable_config(exe.clone()), None);
        std::fs::create_dir(root.join("config")).unwrap();
        assert_eq!(portable_config(exe), Some(root.join("config/config.toml")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_configs_keep_icons_disabled() {
        let config: Config = toml::from_str("exclude = ['.git']").unwrap();
        assert!(!config.icons);
        assert_eq!(config.temp_copy_max_mib, 100);
        let config: Config = toml::from_str("icons = true").unwrap();
        assert!(config.icons);
        assert_eq!(config.exclude, Config::default().exclude);
    }
}
