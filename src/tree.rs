//! Browse drawn as a tree: the folder browse is in at the top, and folders
//! below it opened and closed in place, as in a file tree side panel. A
//! folder's contents are read when it is opened, not before.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::browse::listing;

pub struct Node {
    pub path: PathBuf,
    /// As shown: a folder's name ends in a separator, as in browse.
    pub label: String,
    pub is_dir: bool,
    /// 0 for the root, 1 for what it holds, and so on.
    pub depth: usize,
    pub open: bool,
    /// The lines drawn before the label, such as `│  ├─ `.
    pub guide: String,
    /// Shown only because something below it matched a search, so drawn
    /// dimmed.
    pub dim: bool,
}

pub struct Tree {
    /// The folder at the top. Empty for the list of drives.
    pub root: PathBuf,
    nodes: Vec<Node>,
    pub selected: usize,
    /// Which row is at the top of the window, as in browse.
    pub first: usize,
}

impl Tree {
    /// A tree of `root` with what it holds listed, and `select` selected when
    /// it is one of those; the root line otherwise.
    pub fn new(root: PathBuf, select: Option<&Path>) -> Self {
        let mut nodes = vec![root_node(&root)];
        nodes.extend(children(&root, 1));
        let selected = select
            .and_then(|path| nodes.iter().position(|node| node.path == path))
            .unwrap_or(0);
        let mut tree = Self {
            root,
            nodes,
            selected,
            first: 0,
        };
        tree.relayout();
        tree
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn selected_node(&self) -> &Node {
        &self.nodes[self.selected]
    }

    /// The selected row's path, or none for the list of drives itself.
    pub fn selected_path(&self) -> Option<PathBuf> {
        let path = &self.selected_node().path;
        (!path.as_os_str().is_empty()).then(|| path.clone())
    }

    /// Where Enter goes: the selected folder, or the folder holding the
    /// selected file. Empty on the list of drives itself.
    pub fn target(&self) -> PathBuf {
        let node = self.selected_node();
        if node.is_dir {
            node.path.clone()
        } else {
            node.path
                .parent()
                .map_or_else(|| node.path.clone(), Path::to_path_buf)
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let last = self.nodes.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
    }

    /// Right: opens the selected folder, or steps into it if it is open.
    pub fn right(&mut self) {
        let node = &self.nodes[self.selected];
        if !node.is_dir {
            return;
        }
        if !node.open {
            self.open(self.selected);
        } else if self
            .nodes
            .get(self.selected + 1)
            .is_some_and(|next| next.depth == node.depth + 1)
        {
            self.selected += 1;
        }
    }

    /// Left: closes the selected folder, or goes to the row it sits under.
    /// Says whether the root line was already selected, in which case the
    /// caller moves the root up with `rise`.
    pub fn left(&mut self) -> bool {
        if self.selected == 0 {
            return true;
        }
        if self.nodes[self.selected].open {
            self.close(self.selected);
            return false;
        }
        let depth = self.nodes[self.selected].depth;
        if let Some(parent) = (0..self.selected)
            .rev()
            .find(|&i| self.nodes[i].depth < depth)
        {
            self.selected = parent;
        }
        false
    }

    /// A tree of what a search under `root` matched, best first in `matches`:
    /// each match under the folders it sits in, which are drawn dimmed unless
    /// they match too. The best match starts out selected.
    pub fn from_matches(root: PathBuf, matches: &[(PathBuf, bool)]) -> Self {
        // Every match and the folders between it and the root, by folder.
        let mut below: HashMap<PathBuf, Vec<(PathBuf, bool)>> = HashMap::new();
        let mut listed: HashSet<PathBuf> = HashSet::new();
        for (path, is_dir) in matches {
            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            let mut parent = root.clone();
            let mut steps = relative.components().peekable();
            while let Some(step) = steps.next() {
                let here = parent.join(step);
                let dir = steps.peek().is_some() || *is_dir;
                if listed.insert(here.clone()) {
                    below
                        .entry(parent.clone())
                        .or_default()
                        .push((here.clone(), dir));
                }
                parent = here;
            }
        }
        let matched: HashSet<&Path> = matches.iter().map(|(path, _)| path.as_path()).collect();
        let mut nodes = vec![root_node(&root)];
        emit(&root, 1, &mut below, &matched, &mut nodes);
        let selected = matches
            .first()
            .and_then(|(best, _)| nodes.iter().position(|node| &node.path == best))
            .unwrap_or(0);
        let mut tree = Self {
            root,
            nodes,
            selected,
            first: 0,
        };
        tree.relayout();
        tree
    }

    /// Opens the folders between the root and `path`, and selects it.
    pub fn reveal(&mut self, path: &Path) {
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return;
        };
        let mut here = self.root.clone();
        for step in relative.components() {
            here = here.join(step);
            let Some(index) = self.nodes.iter().position(|node| node.path == here) else {
                return;
            };
            self.selected = index;
            if here != path {
                self.open(index);
            }
        }
    }

    /// Opens or closes the selected folder, for a double click.
    pub fn toggle(&mut self) {
        if self.nodes[self.selected].open && self.selected > 0 {
            self.close(self.selected);
        } else {
            self.open(self.selected);
        }
    }

    /// Makes `parent` the root, with the old root open under it as it was,
    /// branches and all, and selected.
    pub fn rise(&mut self, parent: PathBuf) {
        let old_root = std::mem::replace(&mut self.root, parent.clone());
        let mut old = std::mem::take(&mut self.nodes).into_iter();
        let mut nodes = vec![root_node(&parent)];
        let mut selected = 0;
        for mut node in children(&parent, 1) {
            if node.path == old_root {
                selected = nodes.len();
                node.open = true;
                nodes.push(node);
                old.next(); // the old root line
                nodes.extend(old.by_ref().map(|mut below| {
                    below.depth += 1;
                    below
                }));
            } else {
                nodes.push(node);
            }
        }
        self.nodes = nodes;
        self.selected = selected;
        self.relayout();
    }

    /// Reads the open folders again, keeping which are open and which row is
    /// selected where they still exist.
    pub fn refresh(&mut self) {
        let open: HashSet<PathBuf> = self
            .nodes
            .iter()
            .filter(|node| node.open)
            .map(|node| node.path.clone())
            .collect();
        let selected = self.nodes[self.selected].path.clone();
        self.nodes = vec![root_node(&self.root)];
        self.nodes.extend(children(&self.root, 1));
        let mut i = 1;
        while i < self.nodes.len() {
            if self.nodes[i].is_dir && open.contains(&self.nodes[i].path) {
                self.open(i);
            }
            i += 1;
        }
        self.selected = self
            .nodes
            .iter()
            .position(|node| node.path == selected)
            .unwrap_or(0);
        self.relayout();
    }

    fn open(&mut self, index: usize) {
        let node = &self.nodes[index];
        if !node.is_dir || node.open {
            return;
        }
        let below = children(&node.path, node.depth + 1);
        self.nodes[index].open = true;
        self.nodes.splice(index + 1..index + 1, below);
        self.relayout();
    }

    fn close(&mut self, index: usize) {
        let depth = self.nodes[index].depth;
        let end = (index + 1..self.nodes.len())
            .find(|&i| self.nodes[i].depth <= depth)
            .unwrap_or(self.nodes.len());
        self.nodes.drain(index + 1..end);
        self.nodes[index].open = false;
        if self.selected >= end {
            self.selected -= end - index - 1;
        } else if self.selected > index {
            self.selected = index;
        }
        self.relayout();
    }

    /// Draws the lines in front of each label. A row is the last of its
    /// folder when no row at its depth follows before a shallower one.
    fn relayout(&mut self) {
        let count = self.nodes.len();
        let mut last = vec![false; count];
        let mut seen: Vec<bool> = Vec::new();
        for i in (0..count).rev() {
            let depth = self.nodes[i].depth;
            seen.resize(depth + 1, false);
            last[i] = !seen[depth];
            seen[depth] = true;
        }
        let mut ancestors: Vec<bool> = Vec::new();
        for (i, node) in self.nodes.iter_mut().enumerate() {
            let depth = node.depth;
            ancestors.resize(depth, false);
            let mut guide = String::new();
            for &ended in ancestors.iter().skip(1) {
                guide.push_str(if ended { "   " } else { "│  " });
            }
            if depth > 0 {
                guide.push_str(if last[i] { "└─ " } else { "├─ " });
            }
            node.guide = guide;
            ancestors.push(last[i]);
        }
    }
}

fn root_node(root: &Path) -> Node {
    let label = if root.as_os_str().is_empty() {
        "Drives".to_string()
    } else {
        let shown = root.display().to_string();
        if shown.ends_with(std::path::MAIN_SEPARATOR) {
            shown
        } else {
            format!("{shown}{}", std::path::MAIN_SEPARATOR)
        }
    };
    Node {
        path: root.to_path_buf(),
        label,
        is_dir: true,
        depth: 0,
        open: true,
        guide: String::new(),
        dim: false,
    }
}

fn children(dir: &Path, depth: usize) -> Vec<Node> {
    listing(dir)
        .into_iter()
        .map(|item| Node {
            path: dir.join(&item.name),
            label: item.label(),
            is_dir: item.is_dir,
            depth,
            open: false,
            guide: String::new(),
            dim: false,
        })
        .collect()
}

/// Puts the rows under `dir` in tree order, each followed by what is under
/// it, sorting each folder as browsing does.
fn emit(
    dir: &Path,
    depth: usize,
    below: &mut HashMap<PathBuf, Vec<(PathBuf, bool)>>,
    matched: &HashSet<&Path>,
    nodes: &mut Vec<Node>,
) {
    let Some(mut kids) = below.remove(dir) else {
        return;
    };
    kids.sort_by_cached_key(|(path, is_dir)| (!*is_dir, label_of(path, false).to_lowercase()));
    for (path, is_dir) in kids {
        nodes.push(Node {
            label: label_of(&path, is_dir),
            open: below.contains_key(&path),
            dim: !matched.contains(path.as_path()),
            path: path.clone(),
            is_dir,
            depth,
            guide: String::new(),
        });
        emit(&path, depth + 1, below, matched, nodes);
    }
}

/// A row's name as browse shows it: a folder's ends in a separator. A drive
/// has no file name, so its path is its name.
fn label_of(path: &Path, is_dir: bool) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    if is_dir && !name.ends_with(std::path::MAIN_SEPARATOR) {
        format!("{name}{}", std::path::MAIN_SEPARATOR)
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// root/{a/{deep/, x.txt}, b/, z.txt}
    fn fixture(name: &str) -> PathBuf {
        let root =
            crate::testing::temp_dir().join(format!("tadoru-tree-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a/deep")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/x.txt"), "").unwrap();
        std::fs::write(root.join("z.txt"), "").unwrap();
        root
    }

    fn shown(tree: &Tree) -> Vec<String> {
        tree.nodes()
            .iter()
            .skip(1)
            .map(|node| format!("{}{}", node.guide, node.label))
            .collect()
    }

    #[test]
    fn folders_open_and_close_in_place_with_their_lines() {
        let root = fixture("open");
        let sep = std::path::MAIN_SEPARATOR;
        let mut tree = Tree::new(root.clone(), None);
        assert_eq!(tree.selected, 0, "nothing chosen: the root line");
        assert_eq!(
            shown(&tree),
            [
                format!("├─ a{sep}"),
                format!("├─ b{sep}"),
                "└─ z.txt".into()
            ]
        );

        tree.move_selection(1);
        tree.right();
        assert_eq!(
            shown(&tree),
            [
                format!("├─ a{sep}"),
                format!("│  ├─ deep{sep}"),
                "│  └─ x.txt".into(),
                format!("├─ b{sep}"),
                "└─ z.txt".into(),
            ]
        );
        assert_eq!(tree.selected_node().label, format!("a{sep}"));
        // Right again steps into the open folder.
        tree.right();
        assert_eq!(tree.selected_node().label, format!("deep{sep}"));
        // Left from a closed folder goes to the row it sits under, then closes it.
        assert!(!tree.left());
        assert_eq!(tree.selected_node().label, format!("a{sep}"));
        assert!(!tree.left());
        assert_eq!(shown(&tree).len(), 3);
        // Left on the root line asks for the root to move up.
        assert!(!tree.left());
        assert!(tree.left());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn enter_goes_to_the_folder_or_the_one_holding_the_file() {
        let root = fixture("target");
        let mut tree = Tree::new(root.clone(), Some(&root.join("a")));
        assert_eq!(tree.target(), root.join("a"));
        tree.right();
        tree.move_selection(2); // x.txt
        assert_eq!(tree.selected_path(), Some(root.join("a/x.txt")));
        assert_eq!(tree.target(), root.join("a"));
        tree.selected = 0;
        assert_eq!(tree.target(), root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn closing_keeps_the_selection_on_the_same_row() {
        let root = fixture("close");
        let mut tree = Tree::new(root.clone(), Some(&root.join("a")));
        tree.right(); // a opens: a, deep, x.txt, b, z.txt
        tree.selected = 4; // b
        tree.close(1);
        assert_eq!(tree.selected_node().path, root.join("b"));
        tree.right(); // b holds nothing: stays
        tree.selected = 1;
        tree.right();
        tree.selected = 2; // deep, inside a
        tree.close(1);
        assert_eq!(tree.selected_node().path, root.join("a"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rising_keeps_the_branches_that_were_open() {
        let root = fixture("rise");
        let inner = root.join("a");
        let mut tree = Tree::new(inner.clone(), Some(&inner.join("deep")));
        tree.right(); // deep opens, holding nothing
        tree.rise(root.clone());
        assert_eq!(tree.root, root);
        assert_eq!(tree.selected_node().path, inner);
        assert!(tree.selected_node().open);
        let paths: Vec<PathBuf> = tree.nodes().iter().map(|n| n.path.clone()).collect();
        assert_eq!(
            paths,
            [
                root.clone(),
                inner.clone(),
                inner.join("deep"),
                inner.join("x.txt"),
                root.join("b"),
                root.join("z.txt"),
            ]
        );
        assert!(tree.nodes()[2].open, "deep was open and stays open");
        assert_eq!(tree.nodes()[2].depth, 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matches_are_shown_under_their_folders_with_the_best_selected() {
        let root = PathBuf::from("R");
        let sep = std::path::MAIN_SEPARATOR;
        // Best first: the file deep down, then a folder that also sits on
        // the way to it, then a file at the top.
        let matches = [
            (root.join("src").join("ui").join("tree.rs"), false),
            (root.join("src"), true),
            (root.join("NOTES.md"), false),
            (root.join("docs").join("tree.md"), false),
        ];
        let tree = Tree::from_matches(root.clone(), &matches);
        let rows: Vec<(String, bool)> = tree
            .nodes()
            .iter()
            .skip(1)
            .map(|node| (format!("{}{}", node.guide, node.label), node.dim))
            .collect();
        assert_eq!(
            rows,
            [
                (format!("├─ docs{sep}"), true),
                ("│  └─ tree.md".into(), false),
                (format!("├─ src{sep}"), false),
                (format!("│  └─ ui{sep}"), true),
                ("│     └─ tree.rs".into(), false),
                ("└─ NOTES.md".into(), false),
            ]
        );
        assert_eq!(tree.selected_node().path, matches[0].0, "the best match");
        assert!(tree.nodes()[1].open && !tree.nodes()[2].open);
    }

    #[test]
    fn reveal_opens_the_way_down_and_selects() {
        let root = fixture("reveal");
        let mut tree = Tree::new(root.clone(), None);
        tree.reveal(&root.join("a/deep"));
        assert_eq!(tree.selected_node().path, root.join("a/deep"));
        assert!(
            tree.nodes()
                .iter()
                .any(|n| n.path == root.join("a") && n.open)
        );
        assert!(!tree.selected_node().open, "the target itself stays closed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refresh_reads_open_folders_again_and_keeps_the_selection() {
        let root = fixture("refresh");
        let mut tree = Tree::new(root.clone(), Some(&root.join("a")));
        tree.right();
        tree.move_selection(1); // deep
        std::fs::create_dir(root.join("a/new")).unwrap();
        tree.refresh();
        assert!(tree.nodes().iter().any(|n| n.path == root.join("a/new")));
        assert_eq!(tree.selected_node().path, root.join("a/deep"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
