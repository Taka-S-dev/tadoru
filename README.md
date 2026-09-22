# tadoru

[日本語](README.ja.md)

**tadoru** is a directory jumper that works the same way in cmd.exe, PowerShell and bash.
Type a few letters and press Enter to cd there, or walk the tree one level at a time.
The name is Japanese (辿る) for following a path to where it leads. Written in Rust with ratatui.

- You remember the folder's name: `c openssl`
- You only know a file's name: `cf Cargo.toml` takes you to the folder holding it
- You want to look around: open `c` and press Tab to browse

`c`, `cf`, browse and favorites need nothing else.
`z`, `zi` and recent read the history kept by `zoxide` (if it is missing, the screen says how to get it).

Builds for Windows and Linux on x86_64 are on the [releases](../../releases) page.
There is code for macOS, but it has not been built or tried there.

## How it relates to fd and fzf

tadoru started as two PowerShell functions, `c` and `cf`, built on fd and fzf. It was rewritten
for two reasons: to have the same commands in cmd.exe, and to stop leaving the list, running
`cd ..` and opening it again every time a search had to start one level higher.
It follows fzf's default colours and layout, but uses no code from fd or fzf.

fd and fzf narrow a list down and print the path you chose; the shell does the cd.
If you only use PowerShell or bash and can install fd and fzf, that is often all you need.
tadoru helps when:

- You want the same cd in cmd.exe. Joining fd and fzf in a batch file tends to break on
  `^`, `&`, `|` and quotes
- You want to move the search without leaving it. Left widens it by one level, and a folder
  reached in the three-column view becomes where the search starts. Ctrl+← undoes the move
- You want a single executable, without installing fd and fzf separately
- You want favorites and an action menu on the same screen

## Install

| Command | What it does |
|---|---|
| `tadoru setup` | Adds one block to your PowerShell or bash startup file |
| `tadoru init cmd --out` | Writes `c.cmd` `cf.cmd` `z.cmd` `zi.cmd` |
| `tadoru init powershell --out` | Writes `c.ps1` `cf.ps1` `z.ps1` `zi.ps1` |

The files are written next to `tadoru.exe`. Put a folder after `--out` to write them elsewhere.

For PowerShell and bash, `setup` is all you need. For zsh, add the lines to `~/.zshrc` by hand
(see the [setup guide](docs/guide/setup.md#setting-it-up-by-hand)). cmd.exe has no startup file, so use
`init cmd --out` and add the folder it wrote to PATH. The `.ps1` scripts are for when you would
rather not touch your profile; with zoxide installed, they cannot replace `z` and `zi`.

`setup` shows what it will write and to which file, then asks before writing. `--yes` skips the
question. It tells the shell from the environment. It adds a single block between two markers,
and running it again does not add another. To remove it, delete everything from one marker to
the other.

The names are `c` `cf` `z` `zi` by default. If a command with the same name is already on PATH,
`setup` and `init --out` show where it is. `--cmd j` gives you `j` and `jf` instead, and `--no-z`
leaves zoxide's own `z` and `zi` alone. See the [setup guide](docs/guide/setup.md#changing-the-command-names).

The release zip holds the exe and all eight scripts above, so unzipping it into a folder on PATH
is enough. To run from the repository, build it first: see [Development](#development).

For setting it up by hand, or to see exactly what `init` writes, read the
[setup guide](docs/guide/setup.md).

## Commands

| Command | What it does |
|---|---|
| `c [query]` | Pick a directory under the current one and cd into it |
| `cf [query]` | Pick a file and cd into the folder holding it |
| `z <keywords>` | cd to the best match in the zoxide history (home, with no arguments) |
| `zi [query]` | Pick a directory from the zoxide history and cd into it |
| `c -` | Go back to where you were before the last move made with tadoru (again to return) |

These are shell functions around `tadoru pick`, which prints the chosen path for the shell to cd
to. From a launcher or a shortcut there is no shell waiting for it, so name an action to run on the
chosen folder instead: `tadoru pick --on-accept "Open shell here"` starts a shell there. See
[starting without a shell](docs/guide/setup.md#starting-without-a-shell).

## The screen

Tab goes between the search and browse. Browse goes back to the search used last, so after
`cf` Tab goes between files and browse. Shift-Tab changes the kind of search:
dirs → files → recent → favorites. The mode names on the top border also switch with a click.
In every mode, typing filters the list and Enter cds there.
Esc clears the filter, and pressed again on an empty filter it quits. Ctrl-C always quits.

| Key | What it does |
|---|---|
| Ctrl-Space | Open the list of keys. A plain letter there runs one, such as `s` for favorites; Esc closes it |
| Tab | Go between the search and browse, back to the search used last |
| Shift-Tab | Change to the next kind of search. In browse, go back to the search |
| Ctrl-D / Ctrl-F / Ctrl-R / Ctrl-S | Go straight to dirs, files, recent or favorites (S for starred) |
| Ctrl-B | Pin or unpin the selected folder as a favorite |
| Ctrl-P | Open the action menu for the selected item |
| Ctrl-A | When a scan stopped at its limit, collect the rest |
| F5 | Refresh the list and the preview |
| Left | Widen the search by one level. In browse, go up a level |
| Right | Go into the selection and keep going. From dirs and files it opens browse there; from favorites and recent it goes back to where you opened them from, now at that place. In browse, go down a level |
| Ctrl+← / Ctrl+→ | Step back or forward through the places visited: folders in browse, starting points in a search. Alt+← / Alt+→ do the same, and Ctrl-T goes back as after a tag jump in Vim |

The path under the top border is the folder the search covers. Click a step of it to search from
there instead; what you typed stays.

Moving somewhere in browse and pressing Tab makes that folder where the search starts. The bottom
of the screen says so when it happens, and Ctrl+← puts it back if that was not what you meant.

Browse shows Miller columns: the parent on the left, the current folder in the middle, and what
the selection holds on the right. It is laid out like Finder's column view, ranger and yazi, but it
is only for getting somewhere: it does not copy, delete or rename files. The mode names on the top
border and the path under them can be clicked; clicking a step of the path goes there.

More in [the screen and keys](docs/guide/screen.md) and [the action menu](docs/guide/actions.md).

## Configuration

Configuration is optional; without it, the defaults apply. tadoru does not create the files itself.

| Command | What it does |
|---|---|
| `tadoru config init` | Writes a template `config.toml` |
| `tadoru actions init` | Writes a template `actions.json` |
| `tadoru actions check` | Checks an `actions.json` you have written |

On Windows the files live in `%APPDATA%\tadoru`. tadoru reads them from there, and the `init`
commands above write there. An existing file is never overwritten.

If you **create** a folder named `config` next to `tadoru.exe`, tadoru reads from and writes to
that folder instead, so copying the folder takes your settings with you. tadoru never creates
this folder itself.

```text
tadoru.exe
config/config.toml
config/actions.json
config/favorites.toml
```

`config.toml` sets icons, whether the mouse is used, the size limit for temporary copies,
folders to leave out of scans, and the most items one scan collects.
[The screen and keys](docs/guide/screen.md) explains each setting.

`actions.json` adds items to the Ctrl-P action menu, and each can have a one-letter key.
See [the action menu](docs/guide/actions.md) for the format.

## Development

```text
cargo build --release
```

This builds `target/release/tadoru.exe`. It needs a stable Rust that supports the 2024 edition.
You can set it up straight from there, for example with `target/release/tadoru.exe setup`.

```text
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The integration tests run real shells and check the directory changes, the exit codes and that
the environment is restored. They need PowerShell 7 (`pwsh`) and bash. On Windows they use
Git Bash; if it is not in the usual place, set `TADORU_TEST_BASH` to its path.

To repeat the performance measurements, set `TADORU_BENCH_ROOT` to the folder to measure.

```text
cargo test --release --bin tadoru benchmark_local_tree -- --ignored --nocapture
cargo test --release --bin tadoru benchmark_mode_switch -- --ignored --nocapture
```

On a Windows x86_64 release build, collecting up to the default limit of 200,000 items from `C:\`
took 0.3 to 0.4 seconds from the start of the scan to the final sort order in files, and about 2
seconds in dirs. dirs collects 200,000 folders and nothing else, so it walks a wider part of the
drive. Updating the filter to `src` took around 10 ms. The numbers vary between runs and are not
precise claims. The scan runs in the background, and the list can be used before it finishes.
The mode switch measurement is there to catch slow work coming back onto the drawing thread.
All of these measure internal work, not process startup, the first frame in a real terminal, or
input latency. The screen is only redrawn when something changes, so leaving it open does not
keep a CPU busy.

## License

MIT. See [LICENSE](LICENSE).
