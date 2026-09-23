# Setup in detail

If `tadoru setup` does the job, the [README](../../README.md) is all you need.
This page is for setting things up by hand, and for when something does not work.

## What setup writes

`setup` adds one block, between two markers, to the shell's startup file. For PowerShell:

```text
# >>> tadoru >>>
$tadoruEncoding = [Console]::OutputEncoding
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Invoke-Expression (& 'C:\path\to\tadoru.exe' init powershell | Out-String)
[Console]::OutputEncoding = $tadoruEncoding
Remove-Variable tadoruEncoding
# <<< tadoru <<<
```

For bash, the block holds a single line: `eval "$(/path/to/tadoru init bash)"`.

Running it again does not add a second block. A block with different contents is replaced, and
an identical one is left alone. To remove it, delete everything from one marker to the other.
The file is written through a temporary file, so an interrupted write cannot leave the startup
file half written. If the existing startup file cannot be read as UTF-8, `setup` stops without
writing and says why.

The shell is worked out from the environment. To name it yourself, use `tadoru setup powershell`,
for example.

## Character encoding

`.ps1` files are written with a UTF-8 byte order mark. Windows PowerShell 5.1 reads a `.ps1`
without one in the system code page, so a path containing non-ASCII characters turns into a
different string and points at an executable that does not exist. The same goes for the `.ps1`
files that `init powershell --out` writes.

The block switches `[Console]::OutputEncoding` for the same reason. PowerShell 5.1 also decodes
what an external program prints using the code page, which would break the executable's path
inside the code `init` prints. PowerShell 7 uses UTF-8 for both from the start, so on 7 neither
of these changes anything.

`tadoru setup` writes to the PowerShell 7 profile
(`Documents\PowerShell\Microsoft.PowerShell_profile.ps1`). For 5.1, add the same block to
`Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1` yourself, and save it as UTF-8
with a byte order mark.

## Setting it up by hand

Write it yourself when your dotfiles are under version control, when you cannot change the
startup files on a shared machine, or when you want to decide the load order with zoxide.

zoxide defines `z` and `zi` as aliases, and an alias wins over a function, so the tadoru lines
have to come **after zoxide init**.

| Shell | File | What to write |
|---|---|---|
| PowerShell | `$PROFILE` | The five lines between the markers in [What setup writes](#what-setup-writes). Save as UTF-8 with a byte order mark |
| bash / zsh | `~/.bashrc` / `~/.zshrc` | `eval "$(tadoru init bash)"` |
| cmd.exe | None | `tadoru init cmd --out` (writes the scripts next to the exe) |

`tadoru init <shell>` only prints the setup code; it does not touch any file. Which file it goes
in, and in what order, is yours to decide, so `init` does not guess.

To write your own wrapper, use `tadoru pick --mode <mode>`. It prints the chosen path as one line
on standard output, for you to pass to `cd`. Standard output carries that line and nothing else;
messages and warnings go to standard error.

| Exit code | Meaning |
|---|---|
| 0 | A path was printed |
| 1 | Closed without choosing (Esc / Ctrl-C) |
| 2 | Error. The reason is on standard error |

Change directory only on `0`. A wrapper that does not tell `1` from `2` hides errors.

| Option | Default | Meaning |
|---|---|---|
| `--mode <mode>` | `dirs` | `dirs`, `files` or `browse`, the screen it opens in; Tab and Shift-Tab still change it. `recent` or `favorites` opens that list over browse of the folder |
| `--query <text>` | empty | What the filter starts with. The environment variable `TADORU_QUERY` wins over it |
| `--root <folder>` | current folder | Where the search starts. `@name` is the favorite of that name; so is `@name` as the first word of the query |
| `--select-1` | off | With exactly one candidate, print it without opening the screen |
| `--on-accept <action>` | none | Run an action on the chosen folder instead of printing it; see below |

## Starting without a shell

A program cannot change the folder of the shell that started it, which is why `c` is a shell
function. Started from a launcher, a shortcut or a terminal's own command line, tadoru has no shell
waiting for the path, and printing it does nothing. For that, name an action to run on the chosen
folder instead:

```text
tadoru pick --root C:\work --on-accept "Open shell here"
```

Enter then starts a shell in that folder, on the terminal tadoru was using, and tadoru exits with
the shell's exit code. Any name from [the action menu](actions.md) works, including your own, so the
same picker can open an editor or a new terminal tab. Upper and lower case are not told apart. Nothing
is printed on standard output. A name that matches no action is reported before the screen opens.
`c`, `cf`, `z` and `zi` never pass this option.

Which terminal it opens in is up to whatever starts it. As the target of a Windows shortcut, for
example:

```text
wt -w 0 nt -d C:\work tadoru pick --on-accept "Open shell here"
wezterm-gui start --cwd C:\work -- tadoru pick --on-accept "Open shell here"
```

The first opens a tab in the Windows Terminal window used last, or a new window when there is none.
Windows Terminal keeps a tab open when its program exits with anything but 0, which Esc does; set
`closeOnExit` to `always` in the profile to have it close. The second opens a WezTerm window.

## PowerShell scripts on PATH

To leave your profile alone, you can put `c.ps1` and the others on PATH instead.

```text
tadoru init powershell --out
```

With nothing after `--out`, the scripts are written next to the exe, so everything tadoru writes
stays in one place and one PATH entry covers it. Give a folder, as in `--out C:\path\on\PATH`,
only to write them somewhere else.

This has two limits. With zoxide installed, its `z` and `zi` aliases win, so this way cannot
replace them. And scripts do not run while the execution policy is Restricted. Files unzipped
from a download may be marked as blocked; run `Unblock-File *.ps1` in that folder.

## After an update

Replacing the executable is enough: the scripts look for the executable in their own folder
first. If the shell integration itself has changed, run `tadoru setup` or `tadoru init` again,
then open a new shell or load the setup again.

## Changing the command names

The default names are `c` (directories), `cf` (files), and `z` and `zi` (the zoxide history).
Single-letter names clash easily with other scripts, and `cf` is also the name of the Cloud
Foundry command line tool.

`--cmd` changes the main name; the file picker is that name with `f` added. It works with both
`setup` and `init`, and `setup` writes it into the profile line as well.

```text
tadoru setup --cmd j            # j and jf
tadoru init cmd --out --cmd j   # j.cmd and jf.cmd
```

`setup` and `init --out` show where a command with the same name is already on PATH. For
cmd.exe and for `.ps1` scripts on PATH, whichever comes first on PATH is the one found, so they
also say which that is. PowerShell and bash functions win inside their own shell, but other
shells still run the other command.

## zoxide

`z`, `zi` and recent read what zoxide has recorded. Where zoxide is missing, the screen says how
to install it and what to use instead. `c`, `cf`, browse and favorites need nothing else.

tadoru keeps no history of its own because zoxide already records every directory change through
the shell's cd hook. A history of tadoru's own would hold only the moves made through tadoru, and
would mean giving up the history you already have. When zoxide is available, tadoru records the
folders it takes you to with `zoxide add`.

tadoru replaces zoxide's `z` and `zi` with its own. Its `z` records where you came from, so `c -`
can take you back, and its `zi` picks from tadoru's list. To keep zoxide's own `z` and `zi`, add
`--no-z`. Then open the recent places from `c` with Ctrl-R.
