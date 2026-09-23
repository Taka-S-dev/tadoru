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
file manager. From the keyboard, Ctrl-O does the same, and Ctrl-E shows the selected item in the
file manager, a file inside its folder with the file selected. Actions that start another
application (right-click, Ctrl-O, Ctrl-E and `detach` actions) leave the screen as it is, so their
result is shown at the bottom of the screen. Success and failure have different colours, and both
differ from the key hints that are always there. A success message goes away after 3 seconds; a
failure stays until the next key or click, so it can be read. tadoru's part ends once it has asked
for the application to start, and it cannot tell when the application actually appears, so no
progress is shown.

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

## What typing matches

Typing matches the letters in order with anything between them, so `opsl` finds `openssl`, and
the best matches come first. Upper and lower case are not told apart. The marks fzf uses narrow it:

| Typed | Matches |
|---|---|
| `'openssl` | The letters side by side, as typed |
| `^src` | At the start of the path |
| `.md$` | At the end of the path |
| `!test` | Leaves out paths with `test` in them |

Words with a space between them must all match: `'openssl .h$` finds the headers with `openssl` in
their path. The path is the one shown in the list; in the tree, it is the path from the top line.

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
determined. A search asked to start on one, as `c @net` does when the favorite is a share, opens in
`browse` instead and says why at the bottom of the screen; so does Right on such a place in the
favorites or recent list. From a search that has moved onto one, press Tab to switch to `browse`; F5
does not lift the restriction. Typing in the tree view is refused the same way, and the tree says
so; its folders can still be opened one at a time. The folder the scan root links to is checked too,
and reparse points such as junctions are not followed during a scan. `browse` still works, but
listing and previewing folders does go over the network. Network mounts on Linux and macOS are not
detected.

cmd cannot use a `\\server\share` path as its current folder, so in cmd a chosen folder on a share
is entered with `pushd`: Windows gives the share a free drive letter, such as `Z:`, and cmd goes
there. `popd` comes back to where you were and frees the letter.

## The list of keys

**Ctrl-Space** opens a panel at the bottom right that lists what tadoru can do, one row each:

```text
d → dirs                           ^D
b → browse                        Tab
s → Favorites                      ^S
l → Go into the selection       Right
```

Press the letter at the left to run a row; nothing needs holding down with it. The arrow keys and
Enter, or a click, work too, and Esc, Ctrl-Space again or a click outside closes the panel without
doing anything. The tabs come first and the panel opens on the one you are on. At the right of each
row is the shortcut that does the same with the panel closed, so the panel is also where the
shortcuts are looked up: the key hints on the bottom border only show as many as fit.

Only Ctrl-Space itself has to get through the terminal. If the terminal keeps one of the shortcuts
for itself, the letter in the panel still works. Where Ctrl-Space is taken, by an input method for
example, the shortcuts and the mouse are what is left.

Ctrl-J and Ctrl-K move the selection down and up, and Ctrl-H deletes a letter as Backspace does, in
every list and in the action menu's filter. Some terminals send Ctrl-H for the Backspace key itself,
and there it still deletes. With nothing left to delete, Backspace and Ctrl-H go up: a level in
browse, and in a search one level wider, as Left does. Held down to clear what was typed, they stop
at the empty filter; let go and press again to go up. tadoru hears a key being let go only on
Windows, so elsewhere press again after a pause of about a second and a half, or after any other
key. Ctrl-L does what Right does.

## Modes and favorites

**Tab** goes between the search and browse. The search is dirs or files, and Tab from browse returns
to the one used last: after `cf` it goes between files and browse. Looking for something and looking
around are what you switch between most, so one key joins the two. Browse's key hints name the
search Tab goes back to, as in `Tab: files`.

**Shift-Tab** goes between dirs and files, and in browse between the columns and the tree. Clicking
a mode name on the top border goes straight to that mode, and so does **Ctrl** with the search's
letter: Ctrl-D dirs, Ctrl-F files. These are Ctrl with a letter rather than Alt with a digit because
a terminal may keep the second for switching its own tabs. The tabs are laid out as `[dirs|files]
[browse|tree]`: Shift-Tab steps within a bracket and Tab goes across, so the two searches sit apart
from browse, which comes as columns or as a [tree](#the-tree-view).

Browse opens the folder being searched, with the item selected in dirs or files still selected. It
does not go into the selected folder: that would replace the list you had just been reading, and in
a list nobody has moved, the selection is only the first row, so you would land somewhere you never
chose.

The path row in dirs and files shows the folder the search covers. Click a step of it, or press
**Left**, to search from there instead. The **◀ ▶ ▲** buttons at the left of that row are in the
same place as in browse, and in a search they act on the starting point: where Left in browse goes
up a level, here it widens the search by a level. The filter you typed stays. The search cannot go
above the top of a drive, and the screen says so.

**Right** goes into the selection without leaving tadoru, where Enter would cd there and quit. From
dirs and files it opens browse inside the folder; on a file, the folder holding it with the file
selected. Tab from there goes back to the search, which then starts from that folder.

**Favorites and recent places** are lists of places to go rather than somewhere to be, so they are
not screens with a tab. **Ctrl-S** (S for the starred ones) opens the favorites and **Ctrl-R** the
recent places, as a list at the bottom right over whichever screen you are on, the way the action
menu opens; the same key, Esc or a click outside closes it.

The two also open from the mouse: `★ favorites` and `◷ recent` sit on the top border after the tabs,
outside the brackets so they read as lists rather than as more tabs, each with its icon. A click
opens the list, a click on the other button swaps it, and a click on the same one closes it. A
screen too narrow for them keeps the tabs and drops the buttons, since the keys still work. In the
list, a click selects a place and a double click goes to it, as Right does; Enter, which quits,
stays on the keyboard as it does in every list. Typing filters the list, and Esc clears what was
typed before it closes.

**Enter** cds to the place and quits, as it does everywhere. **Right** takes the screen under the
list to the place and closes the list: looking for something in dirs, open favorites, pick one and
press Right, and the dirs search carries on from that favorite, with the filter cleared since what
was typed picked the place out and would match nothing inside it; Ctrl+← brings back both it and
where the search started. Over browse, or the tree, the place is shown in its folder and selected,
the way Tab shows a search result, so Enter then goes to the favorite itself rather than to whatever
sorts first inside it, and Right again goes into it. A favorite whose folder has been deleted stays
in the list so it can be unpinned, and the bottom of the screen says it is not there.

The keys that act on the selection work in the list too: Ctrl-P opens the action menu for the place,
Ctrl-O and Ctrl-E open it, and in favorites Ctrl-B unpins and Ctrl-N names. A picker opened straight
into a list, as `zi` does, has the list over browse of the folder the shell is in, so Esc leaves
that browse behind. If the history cannot be read, the list shows why, and F5 tries again.

Once opened, browse keeps its folder, selection and filter as you switch modes, and clicking its
mode name again does not reset them. After the search has moved, though, browse lines up with the
new starting point, since coming back to an old folder while the search looks elsewhere would look
like a jump to an unrelated place. F5 refreshes the list and the preview; in browse it keeps the
filter and the selection where it can.

Press **Ctrl-B** to pin or unpin the selected folder as a favorite. Pinned folders show a yellow
**★**, even with icons turned off. Changes made from another shell show up after F5. With a file
selected, its folder is pinned. In files, ★ means the file's folder is a favorite. To go to a
favorite, press **Ctrl-S**, filter and press Enter. zoxide is not needed. Folders that have since
been deleted stay in the list, so they can still be unpinned with Ctrl-B.

Favorites can be managed from the command line too. Without a path, the current folder is used.

```text
tadoru favorite add
tadoru favorite add "C:\work\my project"
tadoru favorite add --name work "C:\work\my project"
tadoru favorite remove "C:\work\my project"
tadoru favorite remove @work
tadoru favorite list
```

A favorite can have a **name**. On the screen, **Ctrl-N** opens a one-line prompt for the selected
folder's name: Enter saves it, Esc leaves things as they were, and saving an empty name takes the
name away while the folder stays pinned; a folder not pinned yet is pinned by naming it. From the
command line the name is given with `--name` when pinning; pinning a pinned folder again with a
name gives it that name. The name is a single word without spaces or slashes, and is matched
without regard to case. Written as `@name`, it stands for the folder wherever a starting point is
given: `c @work` searches from that favorite instead of the current folder, `c @work src` searches
it for `src`, `cf @work` does the same for files, and `--root @work` does it for a picker started by
hand or from a launcher. The favorites list shows the name before the path, so typing the name
finds it there too. A name nobody has given is refused with a message, not searched for.

They are saved in `favorites.toml`, in the same folder as the configuration files, one `[[favorite]]`
table each; a file from before names existed is still read. Saving takes a lock, so pinning from
several shells at once does not lose updates.

## Moving and history in browse

Browse uses [Miller columns](https://en.wikipedia.org/wiki/Miller_columns), the layout that went from
the NeXTSTEP file viewer to the column view of Finder on macOS; ranger and yazi use similar columns.
The parent is on the left, the current folder in the middle, and what the selection holds on the
right. tadoru is a tool for cd, so it does not copy, delete or rename. Only the middle column has a
background and a pointer, so it is always clear which column the cursor is in. The left and right
columns mark the current folder in an accent colour.

On Windows there is no folder above the top of a drive, so going up from one, with Left, Backspace
or ▲, lists the drives, with the one just left selected. Enter goes to that drive and Right goes
into it, so the top of a drive can be chosen like any other folder, and another drive can be picked
from the same list. At the top of a drive the left column lists the drives too. A search started
from the list begins at the drive selected. The list shows the drive letters Windows has assigned,
mapped network drives included, and is made without opening any of them.

**Ctrl+← goes back and Ctrl+→ goes forward.** Alt+← and Alt+→ do the same, but if the terminal uses
Alt with the arrows for something else, such as moving between panes, those keys never reach tadoru,
so use Ctrl. Ctrl-T goes back too, as it does after a tag jump in Vim. Unlike going up a level,
these follow the history of places visited. The same keys work in a search, where they follow the
history of starting points. Moving in browse and pressing Tab moves the starting point, so even a
starting point you wandered into is one key away from being undone. When the starting point changes,
the bottom of the screen names the new place and how to go back.

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

The screen takes the whole window, on the terminal's alternate screen as fzf does without
`--height`. The shell's screen is not scrolled or written over, and it comes back as it was when
tadoru exits.

`c -` goes back to where you were before the last successful `c`, `cf`, `z` or `zi` in the same
shell. Cancelling, a failure, or a move to the same place does not change it, and a plain `cd` is
not recorded. If there is nowhere to go back to, or that place has been deleted, tadoru says why and
stays put.

## The tree view

Browse can also be drawn as a tree, for when the shape of a folder matters more than one level of
it: press Shift-Tab in browse, click **tree** on the top border, or open Ctrl-Space and press `v`.
The same keys, or a click on **browse**, go back to the columns. From a search either tab opens
browse drawn that way, and Tab returns to whichever of the two was used last.

```text
C:\work\project\
├─ docs\
├─ src\
│  ├─ main.rs
│  └─ picker.rs
└─ README.md
```

The top line is the folder browse is in, and below it what that folder holds, in the same order as
the columns. Right opens a folder in place, and on an open folder moves into it; Left closes an open
folder, and otherwise goes to the line it sits under. On the top line Left moves the whole tree up a
level, keeping the branches that were open, and on Windows from the top of a drive to the list of
drives. A double click opens or closes a folder. A folder is read when it is opened, not before.

Enter goes to the selected folder, or to the folder holding the selected file; on the top line it
goes to the folder browse is in. The tree starts on the item that was selected in the columns, and
going back to the columns selects the item chosen in the tree. Moving the top of the tree up is kept
in browse's history, so Ctrl+← and Ctrl-T come back. F5 reads the open folders again and keeps them
open.

Typing in the tree searches everything under its top line, folders and files alike, as the search
screens do. The best matches are shown under the folders they sit in, with those folders dimmed
unless they match too, and the best one is selected. The letters that matched are coloured as in the
lists, in the folders on the way too when the match runs through them; the line under the prompt
says how many matched and how many of them are shown. While the scan is still finding matches the
selection follows the best one, until you move it. Enter goes there. Right leaves the search for the
tree of folders, opened down to the match, so a search can find the place and the tree go on from
there. Esc clears what was typed, and Backspace deletes a letter and then goes up as it does
elsewhere. The search starts when the first letter is typed and follows the same scan limit and
skipped folders as dirs and files.
