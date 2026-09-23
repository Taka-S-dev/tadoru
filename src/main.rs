mod action_menu;
mod actions;
mod browse;
mod config;
mod favorites;
mod icons;
mod key_menu;
mod keys;
mod names;
mod open;
mod picker;
mod scan;
mod setup;
mod shim;
#[cfg(test)]
mod testing;
mod tree;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

/// Directory jumper for cmd.exe, PowerShell and bash.
#[derive(Parser)]
#[command(name = "tadoru", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or validate the user-defined action menu.
    Actions {
        #[command(subcommand)]
        command: ActionsCommand,
    },
    /// Write a commented settings file to edit. Never overwrites an existing one.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Add, remove or list pinned directories (independent of zoxide).
    Favorite {
        #[command(subcommand)]
        command: FavoriteCommand,
    },
    /// Pick a path interactively and print it to stdout.
    Pick(PickArgs),
    /// Put the shell integration in place, after showing what it will write.
    Setup {
        /// Which shell to set up. Detected from the environment when omitted.
        #[arg(value_enum)]
        shell: Option<Shell>,
        /// Write without asking, for an unattended install.
        #[arg(long)]
        yes: bool,
        /// Name the directory picker this instead of c; the file picker adds an f.
        #[arg(long, value_name = "NAME")]
        cmd: Option<String>,
        /// Leave zoxide's own z and zi alone instead of replacing them.
        #[arg(long)]
        no_z: bool,
    },
    /// Print shell integration code for the given shell.
    Init {
        #[arg(value_enum)]
        shell: Shell,
        /// Write c, cf, z and zi as script files (.cmd or .ps1) instead of printing.
        /// Without a directory they are written to the folder holding tadoru.exe, so
        /// every generated file stays in one place and one PATH entry covers them.
        #[arg(long, value_name = "DIR", num_args = 0..=1)]
        out: Option<Option<PathBuf>>,
        /// Name the directory picker this instead of c; the file picker adds an f.
        #[arg(long, value_name = "NAME")]
        cmd: Option<String>,
        /// Leave zoxide's own z and zi alone instead of replacing them.
        #[arg(long)]
        no_z: bool,
    },
}

#[derive(Subcommand)]
enum ActionsCommand {
    Init,
    Check,
}

#[derive(Subcommand)]
enum ConfigCommand {
    Init,
}

#[derive(Subcommand)]
enum FavoriteCommand {
    /// Pin a directory. Defaults to the current directory.
    Add {
        path: Option<PathBuf>,
        /// A name to ask for it by: `c :NAME` searches from it, and so does
        /// `--root :NAME`. Pinning a pinned directory again gives it the name.
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Unpin a directory, including one that no longer exists, or a favorite by name as :NAME.
    Remove { path: Option<PathBuf> },
    /// Print the pinned directories, with their names.
    List,
}

#[derive(clap::Args)]
pub struct PickArgs {
    /// What to list.
    #[arg(long, value_enum, default_value_t = Mode::Dirs)]
    pub mode: Mode,
    /// Initial query. The TADORU_QUERY environment variable takes precedence.
    #[arg(long, default_value = "")]
    pub query: String,
    /// Directory to scan, or a favorite by name as :NAME. Defaults to the
    /// current directory, unless the query starts with :NAME.
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Print the only candidate without showing the picker.
    #[arg(long)]
    pub select_1: bool,
    /// Run this action from the action menu on the chosen folder instead of
    /// printing it, and exit with the action's exit code. For starting tadoru
    /// where no shell function is waiting to cd, such as from a launcher.
    #[arg(long, value_name = "ACTION")]
    pub on_accept: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Directories below the root.
    Dirs,
    /// Files below the root. Prints the parent directory of the chosen file.
    Files,
    /// Recently visited directories from zoxide.
    Recent,
    /// Pinned directories, independent of zoxide.
    Favorites,
    /// Walk the tree one level at a time.
    Browse,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Dirs => "dirs",
            Mode::Files => "files",
            Mode::Recent => "recent",
            Mode::Favorites => "favorites",
            Mode::Browse => "browse",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Shell {
    Powershell,
    Cmd,
    Bash,
}

impl Shell {
    fn label(self) -> &'static str {
        match self {
            Shell::Powershell => "powershell",
            Shell::Cmd => "cmd",
            Shell::Bash => "bash",
        }
    }
}

enum Outcome {
    Path(PathBuf),
    Cancelled,
    Done,
    /// An action ran in place of printing the path, and its exit code is ours.
    Ran(u8),
}

const EXIT_CANCELLED: u8 = 1;
const EXIT_ERROR: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Actions { command } => {
            let result = match command {
                ActionsCommand::Init => actions::init(),
                ActionsCommand::Check => actions::check(),
            };
            result
                .map(|path| {
                    eprintln!("{}", path.display());
                    Outcome::Done
                })
                .map_err(Into::into)
        }
        Command::Config { command } => {
            let ConfigCommand::Init = command;
            config::init().map(|path| {
                eprintln!("{}", path.display());
                Outcome::Done
            })
        }
        Command::Favorite { command } => favorite(command).map(|()| Outcome::Done),
        Command::Pick(args) => pick(args),
        Command::Setup {
            shell,
            yes,
            cmd,
            no_z,
        } => names::Names::new(cmd, no_z)
            .and_then(|names| setup(shell, yes, &names))
            .map(|()| Outcome::Done),
        Command::Init {
            shell,
            out,
            cmd,
            no_z,
        } => names::Names::new(cmd, no_z)
            .and_then(|names| init(shell, out, &names))
            .map(|()| Outcome::Done),
    };
    match result {
        Ok(Outcome::Path(path)) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::Ran(code)) => ExitCode::from(code),
        Ok(Outcome::Cancelled) => ExitCode::from(EXIT_CANCELLED),
        Err(err) => {
            eprintln!("tadoru: {err}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn favorite(command: FavoriteCommand) -> Result<(), Box<dyn std::error::Error>> {
    let file = favorites::path()?;
    let (path, add, name) = match command {
        FavoriteCommand::List => {
            let all = favorites::read(&file)?;
            // The name column is as wide as the longest name, so the paths
            // line up whether or not a favorite has one.
            let width = all
                .iter()
                .filter_map(|f| f.name.as_ref().map(|n| n.len() + 1))
                .max()
                .unwrap_or(0);
            for favorite in all {
                let name = favorite.name.map(|n| format!(":{n}")).unwrap_or_default();
                if width == 0 {
                    println!("{}", favorite.path.display());
                } else {
                    println!("{name:<width$}  {}", favorite.path.display());
                }
            }
            return Ok(());
        }
        FavoriteCommand::Add { path, name } => (path, true, name),
        FavoriteCommand::Remove { path } => (path, false, None),
    };
    let path = match path {
        Some(path) => favorite_or_path(&file, path)?,
        None => std::env::current_dir()?,
    };
    favorites::pin(&file, &path, Some(add), name.as_deref())?;
    eprintln!(
        "{}: {}{}",
        if add { "Pinned" } else { "Unpinned" },
        path.display(),
        name.map(|n| format!(" as :{n}")).unwrap_or_default()
    );
    Ok(())
}

/// `:NAME` is the favorite called NAME; anything else is a path as given.
/// A colon, since PowerShell reads `@NAME` as splatting and passes an empty
/// argument on, while `:NAME` goes through it, cmd and bash as typed.
fn favorite_or_path(file: &Path, given: PathBuf) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let text = given.to_string_lossy();
    let Some(name) = text.strip_prefix(':') else {
        return Ok(given);
    };
    match favorites::find(file, name)? {
        Some(favorite) => Ok(favorite.path),
        None => Err(format!("no favorite named :{name} (tadoru favorite list shows them)").into()),
    }
}

fn pick(mut args: PickArgs) -> Result<Outcome, Box<dyn std::error::Error>> {
    if let Ok(query) = std::env::var("TADORU_QUERY") {
        args.query = query;
    }
    let root = match args.root.take() {
        Some(root) => favorite_or_path(&favorites::path()?, root)?,
        // Typed as the first word, `:NAME` picks where to search from, so a
        // shell command can take it as its own first argument: `c :work src`.
        None => match args.query.split_once(char::is_whitespace) {
            Some((first, rest)) if first.starts_with(':') => {
                let root = favorite_or_path(&favorites::path()?, PathBuf::from(first))?;
                args.query = rest.trim_start().to_string();
                root
            }
            None if args.query.starts_with(':') => {
                let root = favorite_or_path(&favorites::path()?, PathBuf::from(&args.query))?;
                args.query.clear();
                root
            }
            _ => std::env::current_dir()?,
        },
    };
    let root = std::path::absolute(&root)?;
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()).into());
    }
    let config = config::Config::load()?;
    let on_accept = args.on_accept.take();
    // Looked up once before the screen opens, so a misspelt name is reported
    // straight away instead of after a folder has been chosen. The picker only
    // ever returns a folder, so the root stands in for whichever it will be.
    if let Some(name) = &on_accept {
        actions::find(name, &root)?;
    }
    let Some(path) = picker::run(args, root, config)? else {
        return Ok(Outcome::Cancelled);
    };
    let Some(name) = on_accept else {
        return Ok(Outcome::Path(path));
    };
    let action = actions::find(&name, &path)?;
    if action.run_mode() == actions::RunMode::Detach {
        action.execute_detached(&path)?;
        return Ok(Outcome::Ran(0));
    }
    let status = action.execute_terminal(&path)?;
    // Ended by a signal, there is no code to pass on.
    Ok(Outcome::Ran(status.code().map_or(EXIT_ERROR, |code| {
        u8::try_from(code).unwrap_or(u8::MAX)
    })))
}

fn setup(
    shell: Option<Shell>,
    yes: bool,
    names: &names::Names,
) -> Result<(), Box<dyn std::error::Error>> {
    let plan = setup::plan(shell, names)?;
    eprintln!("{}", plan.describe());
    if plan.is_noop() {
        return Ok(());
    }
    if !yes && !setup::confirm()? {
        eprintln!("Nothing was written.");
        return Ok(());
    }
    setup::apply(&plan)?;
    eprintln!("Done. Open a new shell, or reload the file, to pick it up.");
    Ok(())
}

fn init(
    shell: Shell,
    out: Option<Option<PathBuf>>,
    names: &names::Names,
) -> Result<(), Box<dyn std::error::Error>> {
    // `--out` with nothing after it means the folder tadoru already lives in,
    // so the scripts land next to the program rather than somewhere chosen on
    // the spot and forgotten.
    let out = match out {
        Some(Some(dir)) => Some(dir),
        Some(None) => Some(
            std::env::current_exe()?
                .parent()
                .ok_or("cannot locate the folder holding tadoru")?
                .to_path_buf(),
        ),
        None => None,
    };
    let dir = out.clone();
    let written = match (shell, out) {
        (Shell::Powershell, None) => {
            print!("{}", shim::powershell(names)?);
            return Ok(());
        }
        (Shell::Powershell, Some(dir)) => shim::write_powershell(&dir, names)?,
        (Shell::Cmd, None) => {
            for (name, body) in shim::cmd(names)? {
                println!("rem ===== {name}");
                print!("{body}");
            }
            return Ok(());
        }
        (Shell::Cmd, Some(dir)) => shim::write_cmd(&dir, names)?,
        (Shell::Bash, None) => {
            print!("{}", shim::bash(names)?);
            return Ok(());
        }
        (Shell::Bash, Some(_)) => {
            return Err("bash needs functions, so use: eval \"$(tadoru init bash)\"".into());
        }
    };
    for path in written {
        eprintln!("wrote {}", path.display());
    }
    // Said once the scripts exist: when a name is taken elsewhere on PATH,
    // the folder order decides which program answers, and nothing else would
    // tell anyone.
    if let Some(dir) = dir {
        let ext = if matches!(shell, Shell::Cmd) {
            ".cmd"
        } else {
            ".ps1"
        };
        let mut skip = vec![dir.clone()];
        if let Some(home) = std::env::current_exe()?.parent() {
            skip.push(home.to_path_buf());
        }
        let taken = names::taken(names, &skip, Some(&dir));
        if !taken.is_empty() {
            eprintln!("Already on PATH under the same name:");
            for taken in &taken {
                eprintln!("{}", taken.for_scripts(&dir, ext));
            }
            eprintln!("{}", names::RENAME_HINT);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn out_of(args: &[&str]) -> Option<Option<PathBuf>> {
        match Cli::try_parse_from(args).unwrap().command {
            Command::Init { out, .. } => out,
            _ => panic!("not an init command"),
        }
    }

    #[test]
    fn init_writes_beside_the_program_when_out_names_no_directory() {
        // Printing is what the profile block reads, so it stays the default.
        assert_eq!(out_of(&["tadoru", "init", "cmd"]), None);
        // A bare --out keeps the generated scripts with the program instead
        // of in whichever folder came to mind at the time.
        assert_eq!(out_of(&["tadoru", "init", "cmd", "--out"]), Some(None));
        assert_eq!(
            out_of(&["tadoru", "init", "cmd", "--out", "D:/bin"]),
            Some(Some(PathBuf::from("D:/bin")))
        );
    }
}
