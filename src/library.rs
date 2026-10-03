//! IDE-style file tree over a directory of AutoEQ files.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    /// For files: whether it parses as a usable AutoEQ preset.
    pub ok: bool,
}

pub struct Tree {
    pub root: PathBuf,
    pub expanded: HashSet<PathBuf>,
    pub visible: Vec<Node>,
    pub cursor: usize,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root,
            expanded: HashSet::new(),
            visible: Vec::new(),
            cursor: 0,
        };
        tree.refresh();
        tree
    }

    /// Rebuild the flattened visible list from disk and the expanded set.
    pub fn refresh(&mut self) {
        self.visible.clear();
        let root = self.root.clone();
        self.walk(&root, 0);
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<(bool, PathBuf)> = read
            .flatten()
            .filter(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                !name.starts_with('.') && !name.ends_with(".pequi-tmp")
            })
            .map(|e| (e.path().is_dir(), e.path()))
            .collect();
        // directories first, then files, each alphabetical
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (is_dir, path) in entries {
            let ok = is_dir || crate::preset::load_autoeq_file(&path).is_ok();
            self.visible.push(Node {
                path: path.clone(),
                depth,
                is_dir,
                ok,
            });
            if is_dir && self.expanded.contains(&path) {
                self.walk(&path, depth + 1);
            }
        }
    }

    pub fn selected(&self) -> Option<&Node> {
        self.visible.get(self.cursor)
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.visible.len() as isize;
        if len > 0 {
            self.cursor = (self.cursor as isize + delta).clamp(0, len - 1) as usize;
        }
    }

    /// Expand/collapse the selected directory.
    pub fn set_expanded(&mut self, expand: bool) {
        let Some(node) = self.selected().cloned() else {
            return;
        };
        if node.is_dir {
            if expand {
                self.expanded.insert(node.path);
            } else {
                self.expanded.remove(&node.path);
            }
        } else if !expand {
            // collapsing on a file jumps to (and collapses) its parent directory
            if let Some(i) = self.visible[..self.cursor]
                .iter()
                .rposition(|n| n.is_dir && n.depth + 1 == node.depth)
            {
                self.cursor = i;
                let parent = self.visible[i].path.clone();
                self.expanded.remove(&parent);
            }
        }
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_first_and_expansion_shows_children() {
        let root = std::env::temp_dir().join(format!("pequi-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("B")).unwrap();
        std::fs::write(root.join("a.txt"), "Preamp: -1 dB\n").unwrap();
        std::fs::write(root.join("bad"), "nonsense").unwrap();
        std::fs::write(
            root.join("B/x.txt"),
            "Filter 1: ON PK Fc 100 Hz Gain 1 dB Q 1\n",
        )
        .unwrap();

        let mut tree = Tree::new(root.clone());
        let names: Vec<_> = tree
            .visible
            .iter()
            .map(|n| n.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["B", "a.txt", "bad"]);
        assert!(tree.visible[1].ok && !tree.visible[2].ok);

        tree.set_expanded(true);
        assert_eq!(tree.visible.len(), 4);
        assert_eq!(tree.visible[1].depth, 1);

        tree.move_by(1);
        tree.set_expanded(false); // on child file: collapse parent
        assert_eq!(tree.cursor, 0);
        assert_eq!(tree.visible.len(), 3);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
