# The screen and keys

What each mode does, the mouse, icons, and the limits on scanning.
To get started, the [README](../../README.md) is enough.

## Configuration files

Configuration files are optional; without them, the defaults apply. **tadoru does not create them
on its own**, so nothing is left on the disk of anyone who never changed a setting. To get a
template, run:

```text
tadoru config init
```

It writes every setting with a comment. An existing file is never overwritten.

tadoru looks for the folder in this order: `TADORU_CONFIG_DIR`, a folder named `config` next to
`tadoru.exe`, then the user configuration folder. The `config` folder is only used if you created
it: create it empty beforehand, and files are written there from then on. tadoru never creates it.
The user configuration folder is `%APPDATA%\tadoru` on Windows, `~/.config/tadoru` on Linux (under
`XDG_CONFIG_HOME` when that is set), and `~/Library/Application Support/tadoru` on macOS.
`config.toml`, `favorites.toml` and `actions.json` all go in the same folder.

## Settings

Every setting in `config.toml` is optional.

| Setting | Default | What it does |
|---|---|---|
| `icons` | `false` | Shows file type icons. Needs a Nerd Font; see [Icons](#icons) |
| `mouse` | `true` | Uses the mouse. `false` leaves text selection and the wheel to the terminal |
| `temp_copy_max_mib` | `100` | The largest file a temporary copy may be, in MiB. `0` turns temporary copies off; see [the action menu](actions.md) |
| `exclude` | `[".git", "node_modules", "dist", "build", "target"]` | Folder names skipped at any depth while scanning |
| `scan_limit` | `200000` | The most entries one scan collects. `0` removes the limit; see [Scan limit](#scan-limit) |

## Icons

If your terminal uses a [Nerd Font](https://www.nerdfonts.com/), add this to the configuration file
to show icons in the search results, browse and the preview.

```toml
icons = true
```

The default is `false`. tadoru cannot tell whether the font has the icons, so if squares or garbled
characters appear, set `icons = false` again. Installing the font is not enough: the terminal has to
be set to use it. Icons are for display only and never change what is searched or where you go.

Excel (xlsx, xlsm and others), CSV and TSV, Word, PowerPoint, PDF, configuration files, databases,
images, audio, video and archives each get their own icon. Extensions match regardless of case,
and any other extension gets a generic file icon. Icons are coloured by kind: spreadsheets green,
Word blue, PowerPoint orange, PDF red. File name colours and the highlighting of matched
characters stay as they are.

## Mouse and Esc

The lists work with the mouse as well (on by default). Click an item to select it; the wheel moves
the selection three items at a time. Click a mode name at the top to switch to it.

In browse, a click in the middle column selects, and a click on a folder in the left or right
column goes to that folder. Double-click a folder in the middle column (two clicks on the same item
within 500 ms), or press Right, to go into it. The filter applies to the folder the path row
names: double-click C and type `ssl`, and it filters what is inside C. Clicking the current
folder's row in the left column goes up to the parent. Clicking a file in the left or right column
goes to its folder and selects it. The wheel always scrolls the middle column.

In the action menu, press the key at the left of a row, click the row, or press Enter to run it.
The wheel moves the selection. Clicking an item closes the menu and changes what is under the
pointer, so clicks are ignored for 0.3 seconds afterwards; a quick second click cannot land on the
list that was behind the menu. The wheel and the keyboard keep working.

Right-click an item in any column to open it with its default application; a folder opens in the
file manager. From the keyboard, Ctrl-O shows the selected item in the file manager and Ctrl-E
opens it with its default application. Actions that start another application (right-click,
Ctrl-O, Ctrl-E and `detach` actions) leave the screen as it is, so their result is shown at the
bottom of the screen. Success and failure have different colours, and both differ from the key
hints that are always there. A success message goes away after 3 seconds; a failure stays until the
next key or click, so it can be read. tadoru's part ends once it has asked for the application to
start, and it cannot tell when the application actually appears, so no progress is shown.

Ctrl+right-click opens that item's action menu, the same as Ctrl-P. In the left and right columns
it does not change the current folder; the item clicked is the target. The preview on the right of
the search screen answers right-click and Ctrl+right-click too. A left click in the preview switches
to browse, going into a folder, or going to a file's folder and selecting the file. Left clicks and
double-clicks only select and move, in every column, and never start another application. To choose
where the shell goes, press Enter.

To keep the terminal's own text selection and wheel, set `mouse = false` in `config.toml` and start
tadoru again.

In a list, Esc clears the filter while there is one, and quits once the filter is empty. Ctrl-C
quits either way.

When a list is longer than the screen, a thick line over the list's right border (in browse, over
the line right of the middle column) shows which part of it is on screen. If the thick line does
not reach the bottom, there are more items below. Nothing is drawn when everything fits.

## Scan limit

dirs and files keep every path they find in memory until tadoru exits, which is what lets a few
letters filter all of them. Started near the top of a drive, that runs to hundreds of MB, so one
scan collects at most a set number of entries: 200,000 by default. Change it with `scan_limit` in
`config.toml`; `0` removes the limit.

When a scan stops at the limit, **stopped at scan_limit, ^A: collect the rest** appears to the
right of the count, so a partial list never passes for a complete one.

Press **Ctrl-A** there to collect again without the limit, with no need to edit the settings and
start again. The scan starts over, but the filter you typed stays. Modes opened later in the same
session also collect without the limit, and use more memory for it.

When the folder you want does not show up, it is often quicker to go down to its level in browse
and press Tab, which narrows where the search starts.

Going up to `C:\` in browse and pressing Tab makes the whole drive the starting point. That is the
easiest way to hit the limit, so go back somewhere deeper before switching.

## Network drives

On Windows, recursive `dirs` and `files` scans of network locations are refused before they start.
This covers UNC paths, drive letters mapped to network drives, and drives whose type cannot be
determined. When a scan is refused, press Tab to switch to `browse`; F5 does not lift the
restriction. The folder the scan root links to is checked too, and reparse points such as junctions
are not followed during a scan. `browse` still works, but listing and previewing folders does go
over the network. Network mounts on Linux and macOS are not detected.

## Modes and favorites

**Tab** goes between the search and browse. The search is one of dirs, files, recent and favorites,
and Tab from browse returns to the one used last: after `cf` it goes between files and browse, and
after `zi` between recent and browse. Looking for something and looking around are what you switch
between most, so one key joins the two. Browse's key hints name the search Tab goes back to, as in
`Tab: files`.

**Shift-Tab** changes the kind of search, in the order dirs → files → recent → favorites. In browse
it goes back to the search, as Tab does. Clicking a mode name on the top border goes straight to
that mode, and so does **Ctrl** with the search's letter: Ctrl-D dirs, Ctrl-F files, Ctrl-R recent
and Ctrl-S favorites, the starred ones. Shift-Tab only steps forward, so from dirs it takes three
presses to reach favorites, and Ctrl-S takes one. Browse already has a key of its own in Tab. These
are Ctrl with a letter rather than Alt with a digit because a terminal may keep the second for
switching its own tabs. The tabs there are laid out as `[dirs|files|recent|favorites]  [browse]`,
keeping the four searches Shift-Tab steps through apart from browse.

Browse opens the folder being searched, with the item selected in dirs or files still selected. It
does not go into the selected folder: that would replace the list you had just been reading, and in
a list nobody has moved, the selection is only the first row, so you would land somewhere you never
chose. recent and favorites list places other than the folder being searched, so switching to browse
from them opens the same place whatever was selected.

The path row in dirs and files shows the folder the search covers. Click a step of it, or press
**Left**, to search from there instead. The **◀ ▶ ▲** buttons at the left of that row are in the same
place as in browse, and in a search they act on the starting point: where Left in browse goes up a
level, here it widens the search by a level. The filter you typed stays. The search cannot go above
the top of a drive, and the screen says so. recent and favorites do not scan a folder, so they show
where their entries come from instead of a path.

**Right** goes into the selected folder without leaving tadoru, where Enter would cd there and quit.
It opens browse inside that folder; on a file, it opens the folder holding it with the file selected.
Tab from there searches from that folder, so a favorite or a recent folder can be where the next
search starts; Ctrl+← in the search puts the starting point back, as after any other move. A
favorite whose folder has been deleted stays where it is, and the bottom of the screen says so.

Once opened, browse keeps its folder, selection and filter as you switch modes, and clicking its mode
name again does not reset them. After the search has moved, though, browse lines up with the new
starting point, since coming back to an old folder while the search looks elsewhere would look like a
jump to an unrelated place. F5 refreshes the list and the preview; in browse it keeps the filter and
the selection where it can. If the history cannot be read, the screen shows why: press F5 to try
again, or Shift-Tab to go to another list.

Press **Ctrl-B** to pin or unpin the selected folder as a favorite. Pinned folders show a yellow
**★**, even with icons turned off. Changes made from another shell show up after F5. With a file
selected, its folder is pinned. In files, ★ means the file's folder is a favorite. To go to a
favorite, press Shift-Tab until you reach **favorites**, or click favorites on the top border, then
filter and press Enter. zoxide is not needed. Folders that have since been deleted stay in the list,
so they can still be unpinned with Ctrl-B.

Favorites can be managed from the command line too. Without a path, the current folder is used.

```text
tadoru favorite add
tadoru favorite add "C:\work\my project"
tadoru favorite remove "C:\work\my project"
tadoru favorite list
```

They are saved in `favorites.toml`, in the same folder as the configuration files. Saving takes a
lock, so pinning from several shells at once does not lose updates.

## Moving and history in browse

Browse uses [Miller columns](https://en.wikipedia.org/wiki/Miller_columns), the layout that went from
the NeXTSTEP file viewer to the column view of Finder on macOS; ranger and yazi use similar columns.
The parent is on the left, the current folder in the middle, and what the selection holds on the
right. tadoru is a tool for cd, so it does not copy, delete or rename. Only the middle column has a
background and a pointer, so it is always clear which column the cursor is in. The left and right
columns mark the current folder in an accent colour.

**Ctrl+← goes back and Ctrl+→ goes forward.** Alt+← and Alt+→ do the same, but if the terminal uses
Alt with the arrows for something else, such as moving between panes, those keys never reach tadoru,
so use Ctrl. Unlike going up a level, these follow the history of places visited. The same keys work
in a search, where they follow the history of starting points. Moving in browse and pressing Tab
moves the starting point, so even a starting point you wandered into is one key away from being
undone. When the starting point changes, the bottom of the screen names the new place and how to go
back.

Going back also brings back the filter that was typed at the time, and the filter and selection of
the place you return to. Moving somewhere new after going back drops the forward history. The history
lasts only while tadoru runs, keeps up to 100 places in each direction, and skips places that have
been deleted. The mouse's side buttons cannot be received directly by the input library tadoru uses;
to use them, assign Ctrl+← and Ctrl+→ to them in the mouse's own software.

The **◀ back, ▶ forward and ▲ up** buttons at the left of the header do the same as the keys when
clicked. A direction with nowhere to go is shown in grey and does nothing.

The path works as a breadcrumb: click a step to go there. The current folder's name is bold white,
and the steps before it are grey. Parts cut off for lack of room cannot be clicked.

Mode names sit on the top border and the path on the row below. The modes never change, so they
belong on the border; the path changes with every move, so it gets a row of its own. Both can be
clicked.

The screen uses the full height of the terminal. Shell output that was on screen is scrolled up out
of the way, and the screen is put back when tadoru exits; that output stays in the scrollback.

`c -` goes back to where you were before the last successful `c`, `cf`, `z` or `zi` in the same
shell. Cancelling, a failure, or a move to the same place does not change it, and a plain `cd` is
not recorded. If there is nowhere to go back to, or that place has been deleted, tadoru says why and
stays put.
