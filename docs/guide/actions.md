# The action menu

A menu for handing the selected file or folder to another application or script.
tadoru itself does not copy, delete or rename; handing things over is as far as it goes.

**Ctrl-P** opens the actions for the selected file or folder. They run on what was selected when the
menu opened. Esc or Ctrl-P goes back.

The menu is a panel at the bottom right, over the preview, so the list and the item the actions run
on stay in view. It grows to fit its longest action, above a minimum width, and its first line
names the item; a long path keeps its end, where the name is. A terminal too small for the panel
gives the menu the whole screen.

When the menu opens it waits for a key: each row reads `f → Open in file manager`, and pressing the
letter at the left of the arrow runs that action, with no modifier. You can also pick with the arrow
keys and press Enter, or click.

| Key | Action |
|---|---|
| f | Open in file manager |
| v | Open in VS Code |
| c | Copy path |
| s | Open shell here |
| d | Open with default application |
| o | Open temporary copy (TEMP_) |

Five of these start with Open, so `o` goes to the temporary copy, which has no other way in.
Outside the menu, Ctrl-E opens the selection with its default application and Ctrl-O shows it in the
file manager.

Below the list, after a divider, is **t Open temporary copies folder**. It is the one item that does
not act on the selection: it opens the folder where tadoru keeps its temporary copies. Mixed into the
list it would look like an action on the row, so it sits apart. Filtering does not hide it.

**Tab** or **/** moves to the input box, where typing filters the actions. Use it when there are many
of them, or when you only remember a name. Tab goes back to waiting for a key and clears what you
typed, so the next key applies to the whole list again. While you type, the left of each row shows
`alt+f` and so on: holding Alt runs an action while typing, even one the filter hides.

## Built-in actions

Without any configuration you get Open in file manager, Open in VS Code, Copy path and Open shell
here; for a file, Open with default application is shown as well. Open in VS Code needs `code` on
PATH.

**Open shell here** starts a shell in the selected folder, or beside the selected file, on the same
terminal. Leaving the shell brings you back to tadoru. The shell is the one the environment names:
`ComSpec` on Windows and `SHELL` elsewhere. For a different one, add an action of your own such as
`{ "name": "PowerShell here", "program": "pwsh", "args": ["-NoLogo"] }`; the working folder is
already the selected one.

For a file, **Open temporary copy (TEMP_)** creates a new folder inside the system temporary folder
every time, copies the file there as `TEMP_<original name>`, and opens the copy with its default
application. The bottom of the screen shows where the copy went. Copies are kept in `tadoru-copies`
inside the system temporary folder. The item below the divider opens that folder, so you can look
through old copies and delete them by hand; it works even when no file is selected.

A copy is limited to **100 MiB** by default. Change it with `temp_copy_max_mib = 100` in
`config.toml`; `0` turns temporary copies off. The setting is read again each time. The size is
checked before copying, and nothing past the limit is written even if the file grows during the copy.
If the limit is exceeded or the copy fails, the partial copy is deleted and no application starts.
Folders cannot be copied.

The original and earlier copies are never overwritten. Copies are not deleted when tadoru exits, and
nothing is written back to the original, so to keep your edits, use Save As in the application. The
system may delete its temporary folder. Only the one file is copied, so documents that depend on
relative links or on files beside them may behave differently. If another application is saving to
the original, stop it before making the copy.

On Linux, copying a path to the clipboard needs `wl-copy` on Wayland, and `xclip` otherwise.

## Adding your own

To build your own menu, run:

```text
tadoru actions init
tadoru actions check
```

`init` writes a template, and `check` checks what you have written. An existing file is never
overwritten. The template only holds actions that are not built in, so nothing appears twice. It is
built into the exe, so it can be written even when the download has no configuration in it. Until
you have one, the menu shows a line pointing to this command. The same template is in
[examples/config/actions.json](../../examples/config/actions.json). For the portable setup, copy
`examples/config` next to `tadoru.exe` and name the copy `config`.

The file is **`actions.json`**, in the same folder as `config.toml`;
[the screen and keys](screen.md#configuration-files) explains how that folder is found. With a
`config` folder next to `tadoru.exe`, `config.toml`, `favorites.toml` and `actions.json` travel
together. Put your own scripts in `config/scripts` and give `program` a relative path such as
`scripts/my-tool.bat`, and nothing needs changing on another PC. The file is read again every time
the menu opens. Mistakes in the JSON are shown on screen, and the built-in actions and moving around
keep working. Configuration found in the folders you move to is never read or run.

```json
{
  "version": 1,
  "include_defaults": true,
  "actions": [
    {
      "name": "Git status",
      "program": "git",
      "args": ["-C", "{dir}", "status", "--short"],
      "target": "any",
      "run": "terminal",
      "key": "s"
    },
    {
      "name": "My script",
      "program": "scripts/task.ps1",
      "args": ["{path}"],
      "target": "directory",
      "run": "terminal"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `version` | The format version. Currently `1` |
| `name` | The name shown in the menu. Required |
| `program` | The executable or script, without arguments |
| `args` | The arguments, as an array. `{path}` is the target, `{dir}` the target's folder (the parent, for a file), and `{config}` the configuration folder |
| `target` | `any`, `file` or `directory`. Default `any` |
| `run` | `terminal` closes the screen for a moment, shows the output and exit code, and returns on Enter or Esc. `detach` starts it in the background without showing output. Default `terminal` |
| `cwd` | The working folder. Default `{dir}`. A relative path is taken from the configuration folder |
| `key` | One character to press while the menu waits for a key. Leave it out for no key |
| `include_defaults` | `false` shows only your own actions. Default `true` |

`key` must be a single letter or digit. `/` cannot be used, because it moves to the input box. A key
used by a built-in action in the table above is allowed: your action wins, and the built-in one stops
showing its key. One key never runs two actions. If two of your own actions share a key,
`tadoru actions check` names them and stops.

To add an action, copy one `{ ... }` inside the `actions` array and change the name, program and
arguments. Separate items with commas, with no comma after the last one. JSON has no comments.
Strings go in double quotes. Write Windows paths as `C:/tools/task.bat` or `C:\\tools\\task.bat`.
Keep `program` and `args` apart, with one argument per array element. After saving, run
`tadoru actions check`, then open the menu again with Ctrl-P to see the change.

A relative `program` such as `scripts/task.ps1` is taken from the configuration folder. `.ps1` scripts
run with PowerShell 7 as `pwsh -NoProfile -File`. You can also write `"program": "pwsh"` with
`"args": ["-NoProfile", "-File", "scripts/task.ps1", "{path}"]`. Windows `.cmd` and `.bat` files can
be given directly as `program`. Relative paths in ordinary `args` are not converted, so pass files
from the configuration folder as `{config}/scripts/...`; the one exception is the argument right
after PowerShell's `-File`, which is taken from the configuration folder. To pass a literal brace,
write `{{` or `}}`.

The target path is also in the environment variable `TADORU_TARGET`, the target's folder in
`TADORU_DIR`, and the configuration folder in `TADORU_CONFIG`. Arguments are never joined into one
shell command string. Rather than putting the target path into code for `cmd /c` or
`pwsh -Command`, use a script file with arguments, or these environment variables. `detach` only
confirms that the program started, so use `terminal` for actions whose output or errors you want to
read.
