use std::collections::HashMap;
use std::num::NonZeroUsize;

use lru::LruCache;
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

    fn ancestors(&self) -> Vec<Node> {
        let mut ancestors: Vec<Node> = std::iter::successors(self.parent(), Node::parent).collect();
        ancestors.reverse();
        ancestors
    }
}

pub struct Handles {
    limit: NonZeroUsize,
    table: Mutex<Table>,
}

struct Table {
    next: fileid3,
    volumes: HashMap<usize, VolumeHandles>,
}

struct VolumeHandles {
    recent: LruCache<fileid3, Node>,
    ids: HashMap<Node, fileid3>,
}

impl VolumeHandles {
    fn new(limit: NonZeroUsize) -> Self {
        Self {
            recent: LruCache::new(limit),
            ids: HashMap::new(),
        }
    }

    fn insert(&mut self, id: fileid3, node: Node) {
        if let Some((evicted_id, evicted)) = self.recent.push(id, node.clone())
            && self.ids.get(&evicted) == Some(&evicted_id)
        {
            self.ids.remove(&evicted);
        }
        self.ids.insert(node, id);
    }

    fn resolve(&mut self, id: fileid3) -> Option<Node> {
        let node = self.recent.peek(&id)?.clone();
        node.ancestors()
            .iter()
            .filter_map(|ancestor| self.ids.get(ancestor).copied())
            .for_each(|ancestor| {
                self.recent.get(&ancestor);
            });
        self.recent.get(&id);
        Some(node)
    }

    fn invalidate(&mut self, node: &Node) {
        if let Some(id) = self.ids.remove(node) {
            self.recent.pop(&id);
        }
    }
}

impl Handles {
    pub fn new(limit: NonZeroUsize) -> Self {
        Self {
            limit,
            table: Mutex::new(Table {
                next: FIRST_ID,
                volumes: HashMap::new(),
            }),
        }
    }

    pub fn limit(&self) -> usize {
        self.limit.get()
    }

    pub fn node(&self, id: fileid3) -> Option<Node> {
        self.table
            .lock()
            .volumes
            .values_mut()
            .find(|volume| volume.recent.contains(&id))
            .and_then(|volume| volume.resolve(id))
    }

    pub fn id_of(&self, node: Node) -> fileid3 {
        let mut table = self.table.lock();
        let Table { next, volumes } = &mut *table;
        let volume = volumes
            .entry(node.volume)
            .or_insert_with(|| VolumeHandles::new(self.limit));
        if let Some(id) = volume.ids.get(&node).copied() {
            volume.recent.get(&id);
            return id;
        }
        let id = *next;
        *next += 1;
        volume.insert(id, node);
        id
    }

    pub fn invalidate(&self, node: &Node) {
        if let Some(volume) = self.table.lock().volumes.get_mut(&node.volume) {
            volume.invalidate(node);
        }
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

    fn handles(limit: usize) -> Handles {
        Handles::new(NonZeroUsize::new(limit).unwrap())
    }

    #[test]
    fn the_same_path_keeps_its_id() {
        let handles = handles(16);
        let id = handles.id_of(node("a/b"));
        assert_eq!(handles.id_of(node("a/b")), id);
        assert_eq!(handles.node(id), Some(node("a/b")));
        assert_ne!(id, ROOT);
    }

    #[test]
    fn invalidating_a_path_drops_only_its_own_handle_like_go() {
        let handles = handles(16);
        let dir = handles.id_of(node("a"));
        let inside = handles.id_of(node("a/b"));
        handles.invalidate(&node("a"));
        assert_eq!(handles.node(dir), None);
        assert_eq!(handles.node(inside), Some(node("a/b")));
        assert_ne!(handles.id_of(node("a")), dir);
    }

    #[test]
    fn parents_walk_up_to_the_volume_root() {
        assert_eq!(node("a/b").parent(), Some(node("a")));
        assert_eq!(node("a").parent(), Some(node("")));
        assert_eq!(node("").parent(), None);
        assert_eq!(node("").child("x"), node("x"));
        assert_eq!(node("a/b/c").ancestors(), vec![node(""), node("a"), node("a/b")]);
    }

    #[test]
    fn the_least_recently_used_handle_goes_stale_past_the_limit() {
        let handles = handles(2);
        let first = handles.id_of(node("a"));
        let second = handles.id_of(node("b"));
        handles.node(first);
        let third = handles.id_of(node("c"));

        assert_eq!(handles.node(second), None);
        assert_eq!(handles.node(first), Some(node("a")));
        assert_eq!(handles.node(third), Some(node("c")));
        assert_ne!(handles.id_of(node("b")), second);
    }

    #[test]
    fn using_a_file_keeps_its_directories_fresh() {
        let handles = handles(3);
        let dir = handles.id_of(node("d"));
        let file = handles.id_of(node("d/f"));
        let other = handles.id_of(node("o"));
        handles.node(file);
        handles.id_of(node("n"));

        assert_eq!(handles.node(other), None);
        assert_eq!(handles.node(dir), Some(node("d")));
        assert_eq!(handles.node(file), Some(node("d/f")));
    }

    #[test]
    fn each_volume_has_its_own_limit() {
        let handles = handles(1);
        let first = handles.id_of(node("a"));
        let elsewhere = handles.id_of(Node {
            volume: 1,
            path: "a".to_owned(),
        });

        assert_eq!(handles.node(first), Some(node("a")));
        assert!(handles.node(elsewhere).is_some());
    }
}
