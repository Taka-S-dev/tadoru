//! User-owned favorites, separate from zoxide's automatically ranked history.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

type Error = Box<dyn std::error::Error>;

#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Favorites {
    paths: Vec<PathBuf>,
}

/// Cached membership for drawing. Lookups never access the filesystem.
#[derive(Default)]
pub struct Index(HashSet<String>);

impl Index {
    pub fn load() -> Result<Self, Error> {
        Self::read(&path()?)
    }

    pub fn read(file: &Path) -> Result<Self, Error> {
        Ok(Self(
            read(file)?.iter().map(|path| display_key(path)).collect(),
        ))
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.0.contains(&display_key(path))
    }
}

fn display_key(path: &Path) -> String {
    #[cfg(windows)]
    {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().trim_end_matches('/').to_string()
    }
}

pub fn path() -> Result<PathBuf, Error> {
    Ok(crate::config::Config::path()
        .ok_or("cannot locate the user configuration directory")?
        .with_file_name("favorites.toml"))
}

pub fn read(path: &Path) -> Result<Vec<PathBuf>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display()).into()),
    };
    let favorites: Favorites =
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(favorites.paths)
}

fn normalized(path: &Path) -> Result<PathBuf, Error> {
    let absolute = std::path::absolute(path)?;
    let path = if absolute.exists() {
        fs::canonicalize(absolute)?
    } else {
        absolute
    };
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(path)
}

fn same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

/// None toggles membership; Some(true/false) makes add/remove idempotent.
/// Writers lock across read/modify/write, and readers see a complete old or new file.
pub fn update(file: &Path, directory: &Path, wanted: Option<bool>) -> Result<bool, Error> {
    let directory = normalized(directory)?;
    let parent = file
        .parent()
        .ok_or("favorites file has no parent directory")?;
    fs::create_dir_all(parent)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(file.with_extension("lock"))?;
    lock.lock()?;
    let mut paths = read(file)?;
    let present = paths.iter().any(|path| same_path(path, &directory));
    let add = wanted.unwrap_or(!present);
    if add && !directory.is_dir() {
        return Err(format!("not a directory: {}", directory.display()).into());
    }
    if add == present {
        return Ok(add);
    }
    if add {
        paths.push(directory);
    } else {
        paths.retain(|path| !same_path(path, &directory));
    }
    let text = toml::to_string(&Favorites { paths })?;
    let temp = file.with_extension("tmp");
    let mut output = fs::File::create(&temp)?;
    output.write_all(text.as_bytes())?;
    output.sync_all()?;
    drop(output);
    fs::rename(&temp, file)?;
    Ok(add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_writers_do_not_lose_favorites() {
        let root = std::env::temp_dir().join(format!(
            "tadoru-favorites-concurrent-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let file = root.join("favorites.toml");
        std::thread::scope(|scope| {
            for index in 0..8 {
                let directory = root.join(index.to_string());
                fs::create_dir(&directory).unwrap();
                let file = &file;
                scope.spawn(move || {
                    update(file, &directory, Some(true)).unwrap();
                });
            }
        });
        assert_eq!(read(&file).unwrap().len(), 8);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn favorites_persist_without_duplicates_and_stale_entries_can_be_removed() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-favorites-{}", std::process::id()));
        fs::create_dir_all(root.join("日本語 folder")).unwrap();
        let file = root.join("favorites.toml");
        let directory = root.join("日本語 folder");
        assert!(update(&file, &directory, Some(true)).unwrap());
        assert!(update(&file, &directory.join("."), Some(true)).unwrap());
        assert_eq!(read(&file).unwrap().len(), 1);
        let index = Index::read(&file).unwrap();
        assert!(index.contains(&directory));
        #[cfg(windows)]
        assert!(index.contains(Path::new(
            &format!("{}/", directory.to_string_lossy().replace('\\', "/").to_uppercase())
        )));
        fs::remove_dir(&directory).unwrap();
        assert!(index.contains(&directory));
        assert!(!update(&file, &directory, None).unwrap());
        assert!(!Index::read(&file).unwrap().contains(&directory));
        assert!(read(&file).unwrap().is_empty());
        assert!(update(&file, &directory, Some(true)).is_err());
        fs::write(&file, "broken [toml").unwrap();
        assert!(update(&file, &root, Some(true)).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "broken [toml");
        fs::remove_dir_all(root).unwrap();
    }
}
