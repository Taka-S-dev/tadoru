//! User-owned favorites, separate from zoxide's automatically ranked history.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

type Error = Box<dyn std::error::Error>;

/// A pinned folder, with the name it can be asked for by, if it has one.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Favorite {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The file: a table per favorite, so a favorite can gain fields without
/// the file changing shape again. `paths` is the form written before names
/// existed; it is still read, and written back in the new form.
#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Favorites {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    paths: Vec<PathBuf>,
    #[serde(rename = "favorite", skip_serializing_if = "Vec::is_empty")]
    favorites: Vec<Favorite>,
}

/// What a name has to be to stand for a favorite: a word with no spaces,
/// not starting with the @ that marks it when it is used, and no path
/// separators, so it can never be taken for a folder.
pub fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() {
        return Err("a favorite's name cannot be empty".into());
    }
    if name.starts_with('@') {
        return Err(
            "a favorite's name is written without the @; that is added when it is used".into(),
        );
    }
    if name
        .chars()
        .any(|c| c.is_whitespace() || c == '/' || c == '\\')
    {
        return Err(format!("a favorite's name cannot hold spaces or slashes: {name}").into());
    }
    Ok(())
}

fn same_name(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

/// The favorite called `name`, if there is one. Names are matched without
/// regard to case, as folder names are on Windows.
pub fn find(file: &Path, name: &str) -> Result<Option<Favorite>, Error> {
    Ok(read(file)?
        .into_iter()
        .find(|favorite| favorite.name.as_deref().is_some_and(|n| same_name(n, name))))
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
            read(file)?
                .iter()
                .map(|favorite| display_key(&favorite.path))
                .collect(),
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

/// Every favorite, in the order they were pinned; the ones from a file
/// written before names existed come first.
pub fn read(path: &Path) -> Result<Vec<Favorite>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", path.display()).into()),
    };
    let favorites: Favorites =
        toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut all: Vec<Favorite> = favorites
        .paths
        .into_iter()
        .map(|path| Favorite { path, name: None })
        .collect();
    all.extend(favorites.favorites);
    Ok(all)
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
    pin(file, directory, wanted, None)
}

/// `update` with a name for the favorite. Adding a folder that is pinned
/// already gives it the name, so a favorite can be named after the fact.
/// A name in use by another favorite is refused rather than moved.
pub fn pin(
    file: &Path,
    directory: &Path,
    wanted: Option<bool>,
    name: Option<&str>,
) -> Result<bool, Error> {
    if let Some(name) = name {
        check_name(name)?;
    }
    let directory = normalized(directory)?;
    let _lock = lock(file)?;
    let mut favorites = read(file)?;
    let present = favorites
        .iter()
        .any(|favorite| same_path(&favorite.path, &directory));
    let add = wanted.unwrap_or(!present);
    if add && !directory.is_dir() {
        return Err(format!("not a directory: {}", directory.display()).into());
    }
    if add
        && let Some(name) = name
        && let Some(taken) = favorites.iter().find(|favorite| {
            favorite.name.as_deref().is_some_and(|n| same_name(n, name))
                && !same_path(&favorite.path, &directory)
        })
    {
        return Err(format!("@{name} already names {}", taken.path.display()).into());
    }
    if add == present && (!add || name.is_none()) {
        return Ok(add);
    }
    if add && present {
        for favorite in &mut favorites {
            if same_path(&favorite.path, &directory) {
                favorite.name = name.map(str::to_string);
            }
        }
    } else if add {
        favorites.push(Favorite {
            path: directory,
            name: name.map(str::to_string),
        });
    } else {
        favorites.retain(|favorite| !same_path(&favorite.path, &directory));
    }
    write(file, favorites)?;
    Ok(add)
}

/// Gives the favorite for `directory` the name, or with None takes its name
/// away; the folder stays pinned. A folder not pinned yet is pinned by a
/// name, and left alone by None.
pub fn set_name(file: &Path, directory: &Path, name: Option<&str>) -> Result<(), Error> {
    let Some(_) = name else {
        let directory = normalized(directory)?;
        let lock = lock(file)?;
        let mut favorites = read(file)?;
        for favorite in &mut favorites {
            if same_path(&favorite.path, &directory) {
                favorite.name = None;
            }
        }
        write(file, favorites)?;
        drop(lock);
        return Ok(());
    };
    pin(file, directory, Some(true), name).map(|_| ())
}

fn lock(file: &Path) -> Result<fs::File, Error> {
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
    Ok(lock)
}

/// Replaces the file in one step, so a reader sees the old list or the new.
fn write(file: &Path, favorites: Vec<Favorite>) -> Result<(), Error> {
    let text = toml::to_string(&Favorites {
        paths: Vec::new(),
        favorites,
    })?;
    let temp = file.with_extension("tmp");
    let mut output = fs::File::create(&temp)?;
    output.write_all(text.as_bytes())?;
    output.sync_all()?;
    drop(output);
    fs::rename(&temp, file)?;
    Ok(())
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
    fn names_are_kept_found_and_refused_when_taken() {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-favnames-{}", std::process::id()));
        let (a, b) = (root.join("a"), root.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let file = root.join("favorites.toml");
        // The form written before names existed is still read, first.
        fs::write(&file, format!("paths = [{:?}]\n", a.to_string_lossy())).unwrap();
        assert!(pin(&file, &b, Some(true), Some("work")).unwrap());
        let all = read(&file).unwrap();
        assert_eq!(
            all[0],
            Favorite {
                path: a.clone(),
                name: None
            }
        );
        assert_eq!(all[1].name.as_deref(), Some("work"));
        // Written back as a table per favorite.
        assert!(fs::read_to_string(&file).unwrap().contains("[[favorite]]"));
        // Found without regard to case; a name in use is refused for another folder.
        assert_eq!(
            find(&file, "WORK").unwrap().map(|f| f.path),
            Some(b.clone())
        );
        assert!(find(&file, "home").unwrap().is_none());
        assert!(pin(&file, &a, Some(true), Some("work")).is_err());
        // Pinning again with a name names the favorite; the list stays one each.
        assert!(pin(&file, &a, Some(true), Some("first")).unwrap());
        assert_eq!(read(&file).unwrap().len(), 2);
        assert_eq!(
            find(&file, "first").unwrap().map(|f| f.path),
            Some(a.clone())
        );
        // A name can be taken away again; the folder stays pinned.
        set_name(&file, &a, None).unwrap();
        assert!(find(&file, "first").unwrap().is_none());
        assert_eq!(read(&file).unwrap().len(), 2);
        set_name(&file, &a, Some("again")).unwrap();
        assert!(find(&file, "again").unwrap().is_some());
        // A name can be taken away again; the folder stays pinned.
        set_name(&file, &a, None).unwrap();
        assert!(find(&file, "first").unwrap().is_none());
        assert_eq!(read(&file).unwrap().len(), 2);
        set_name(&file, &a, Some("again")).unwrap();
        assert!(find(&file, "again").unwrap().is_some());
        for bad in ["", "@work", "my work", "a/b"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
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
