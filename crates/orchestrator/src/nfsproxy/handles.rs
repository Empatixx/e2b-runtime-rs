use std::collections::HashMap;

use nfsserve::nfs::fileid3;
use parking_lot::Mutex;

pub const ROOT: fileid3 = 1;
const FIRST_ID: fileid3 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Node {
    pub volume: usize,
    pub path: String,
}

impl Node {
    pub fn volume_root(volume: usize) -> Self {
        Self {
            volume,
            path: String::new(),
        }
    }

    pub fn child(&self, name: &str) -> Self {
        Self {
            volume: self.volume,
            path: match self.path.is_empty() {
                true => name.to_owned(),
                false => format!("{}/{name}", self.path),
            },
        }
    }

    pub fn parent(&self) -> Option<Self> {
        (!self.path.is_empty()).then(|| Self {
            volume: self.volume,
            path: self.path.rsplit_once('/').map_or("", |(parent, _)| parent).to_owned(),
        })
    }

    fn contains(&self, other: &Node) -> bool {
        self.volume == other.volume
            && (other.path == self.path || self.path.is_empty() || other.path.starts_with(&format!("{}/", self.path)))
    }

    fn moved(&self, from: &Node, to: &Node) -> Node {
        Node {
            volume: to.volume,
            path: format!("{}{}", to.path, &self.path[from.path.len()..]),
        }
    }
}

pub struct Handles {
    table: Mutex<Table>,
}

struct Table {
    next: fileid3,
    nodes: HashMap<fileid3, Node>,
    ids: HashMap<Node, fileid3>,
}

impl Default for Handles {
    fn default() -> Self {
        Self {
            table: Mutex::new(Table {
                next: FIRST_ID,
                nodes: HashMap::new(),
                ids: HashMap::new(),
            }),
        }
    }
}

impl Handles {
    pub fn node(&self, id: fileid3) -> Option<Node> {
        self.table.lock().nodes.get(&id).cloned()
    }

    pub fn id_of(&self, node: Node) -> fileid3 {
        let mut table = self.table.lock();
        if let Some(id) = table.ids.get(&node) {
            return *id;
        }
        let id = table.next;
        table.next += 1;
        table.nodes.insert(id, node.clone());
        table.ids.insert(node, id);
        id
    }

    pub fn forget(&self, gone: &Node) {
        let mut table = self.table.lock();
        let ids: Vec<fileid3> = table
            .ids
            .iter()
            .filter(|(node, _)| gone.contains(node))
            .map(|(_, id)| *id)
            .collect();
        ids.iter().for_each(|id| {
            if let Some(node) = table.nodes.remove(id) {
                table.ids.remove(&node);
            }
        });
    }

    pub fn moved(&self, from: &Node, to: &Node) {
        self.forget(to);
        let mut table = self.table.lock();
        let moved: Vec<(fileid3, Node)> = table
            .nodes
            .iter()
            .filter(|(_, node)| from.contains(node))
            .map(|(id, node)| (*id, node.moved(from, to)))
            .collect();
        moved.into_iter().for_each(|(id, node)| {
            if let Some(old) = table.nodes.insert(id, node.clone()) {
                table.ids.remove(&old);
            }
            table.ids.insert(node, id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str) -> Node {
        Node {
            volume: 0,
            path: path.to_owned(),
        }
    }

    #[test]
    fn the_same_path_keeps_its_id() {
        let handles = Handles::default();
        let id = handles.id_of(node("a/b"));
        assert_eq!(handles.id_of(node("a/b")), id);
        assert_eq!(handles.node(id), Some(node("a/b")));
        assert_ne!(id, ROOT);
    }

    #[test]
    fn forgetting_a_directory_forgets_what_is_inside() {
        let handles = Handles::default();
        let dir = handles.id_of(node("a"));
        let inside = handles.id_of(node("a/b"));
        let sibling = handles.id_of(node("ab"));
        handles.forget(&node("a"));
        assert_eq!((handles.node(dir), handles.node(inside)), (None, None));
        assert_eq!(handles.node(sibling), Some(node("ab")));
    }

    #[test]
    fn a_rename_moves_the_ids_of_the_tree() {
        let handles = Handles::default();
        let dir = handles.id_of(node("a"));
        let inside = handles.id_of(node("a/b"));
        let replaced = handles.id_of(node("c"));
        handles.moved(&node("a"), &node("c"));
        assert_eq!(handles.node(dir), Some(node("c")));
        assert_eq!(handles.node(inside), Some(node("c/b")));
        assert_eq!(handles.node(replaced), None);
        assert_eq!(handles.id_of(node("c/b")), inside);
    }

    #[test]
    fn parents_walk_up_to_the_volume_root() {
        assert_eq!(node("a/b").parent(), Some(node("a")));
        assert_eq!(node("a").parent(), Some(node("")));
        assert_eq!(node("").parent(), None);
        assert_eq!(node("").child("x"), node("x"));
    }
}
