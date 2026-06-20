use std::hash::{BuildHasher, Hash};
use std::mem::replace;
use std::ops::{Index, IndexMut};
use std::time::SystemTime;

use bitflags::bitflags;
use ecow::EcoVec;
use hashbrown::hash_table::Entry;
use hashbrown::{DefaultHashBuilder, HashTable};
use walkdir::WalkDir;

use crate::config::Filter;
use crate::events::EventType;
use crate::metadata::Metadata;
use crate::path::CanonicalPathBuf;
use crate::pending::{self, PendingChange, PendingChanges};

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeMeta {
    Dir,
    File { mtime: SystemTime, size: usize },
    Deleted,
}

impl NodeMeta {
    fn is_dir(&self) -> bool {
        matches!(self, NodeMeta::Dir)
    }

    fn is_file(&self) -> bool {
        matches!(self, NodeMeta::File { .. })
    }

    pub fn new(meta: &Metadata) -> NodeMeta {
        if meta.is_dir {
            NodeMeta::Dir
        } else {
            NodeMeta::File {
                mtime: meta.mtime,
                size: meta.size,
            }
        }
    }

    fn change_type(&self, new: &Self, skip_check: bool) -> Option<EventType> {
        // we only care for changes that inolve a file, ingnore everything else
        match (&self, &new) {
            (
                NodeMeta::File { mtime, size },
                NodeMeta::File {
                    mtime: nmtime,
                    size: nsize,
                },
            ) => {
                if !skip_check && mtime == nmtime && size == nsize {
                    None
                } else {
                    Some(EventType::Modified)
                }
            }
            (NodeMeta::Deleted | NodeMeta::Dir, NodeMeta::File { .. }) => Some(EventType::Create),
            (NodeMeta::File { .. }, NodeMeta::Deleted | NodeMeta::Dir) => Some(EventType::Delete),
            _ => None,
        }
    }
}

#[derive(PartialEq, Eq, PartialOrd, Clone, Copy, Hash, Debug)]
pub struct DirId(u32);

impl DirId {
    const NONE: DirId = DirId(u32::MAX);

    pub fn is_none(self) -> bool {
        self == Self::NONE
    }

    pub fn is_some(self) -> bool {
        self != Self::NONE
    }

    #[inline]
    pub fn idx(self) -> usize {
        // A `debug_assert` let `NONE` (u32::MAX) reach `self.dirs[..]` in release,
        // a baffling OOB far from the cause. Fail at the source; callers guard with
        // `is_some()` first.
        assert!(
            self.is_some(),
            "filesentry bug: DirId::NONE used as a tree index"
        );
        self.0 as usize
    }
}
impl From<usize> for DirId {
    fn from(value: usize) -> Self {
        assert!(value < Self::NONE.0 as usize);
        DirId(value as u32)
    }
}

#[derive(PartialEq, Eq, PartialOrd, Clone, Copy, Hash, Debug)]
pub struct NodeId(u32);

impl NodeId {
    const NONE: NodeId = NodeId(u32::MAX);

    pub fn is_some(self) -> bool {
        self != Self::NONE
    }

    pub fn is_none(self) -> bool {
        self == Self::NONE
    }

    #[inline]
    pub fn idx(self) -> usize {
        // As `DirId::idx`: guard `NONE` in release too, not just under `debug_assert`.
        assert!(
            self.is_some(),
            "filesentry bug: NodeId::NONE used as a tree index"
        );
        self.0 as usize
    }
}

impl From<usize> for NodeId {
    fn from(value: usize) -> Self {
        assert!(value < u32::MAX as usize);
        NodeId(value as u32)
    }
}

const _ASSERT: () = {
    if size_of::<FsNode>() != 7 * 8 {
        panic!("size of FsNode must stay constant")
    }
};

bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct Flags: u32 {
        /// temporary flag used during recursive crawls to tag which children
        /// weren't visited yet (and mark those as deleted in the end)
        const MAYBE_DELETED = 1;
        /// wether to watch the chidren of this node, this is always
        /// true for recursive watches but for non recursive watches
        /// it's only true for the top dir
        const WATCH_CHILDREN = 0b10;
        /// wether this node is being watched recursively
        const RECURSIVE = 0b110;
    }
}

#[derive(Debug)]
pub struct FsNode {
    pub path: CanonicalPathBuf, // 2 words
    meta: NodeMeta,             // 3 words
    inode: u64,                 // 1 word
    flags: Flags,               // 1 word
    children: DirId,
}

impl FsNode {
    pub fn set_maybe_deleted_flag(&mut self) {
        self.flags |= Flags::MAYBE_DELETED;
    }

    pub fn maybe_deleted_flag(&self) -> bool {
        self.flags.contains(Flags::MAYBE_DELETED)
    }

    pub fn unset_maybe_deleted_flag(&mut self) {
        self.flags.remove(Flags::MAYBE_DELETED)
    }
}

pub struct FileTree {
    path_table: HashTable<NodeId>,
    hasher: DefaultHashBuilder,
    nodes: Vec<FsNode>,
    dirs: Vec<EcoVec<NodeId>>,
}

impl FileTree {
    pub fn new() -> Self {
        Self {
            path_table: HashTable::with_capacity(1024),
            hasher: DefaultHashBuilder::default(),
            nodes: Vec::with_capacity(1024),
            dirs: Vec::with_capacity(128),
        }
    }

    pub fn apply_transaction(
        &mut self,
        transaction: &mut PendingChanges,
        filter: &dyn Filter,
        mut emit_event: impl FnMut(CanonicalPathBuf, EventType),
        work_stack: &mut Vec<(NodeId, usize)>,
        mut add_watch: impl FnMut(CanonicalPathBuf),
    ) {
        let mut transaction = transaction.drain().peekable();
        while let Some(change) = transaction.next() {
            let (node, recurse) = self.apply_change(&change, work_stack, &mut emit_event);
            if recurse {
                if node.is_some()
                    && self[node].meta.is_dir()
                    // double check that this path is not ignored before dowing an expensive crawl
                    && !filter.ignore_path(change.path.as_std_path(), Some(true))
                {
                    self.crawl(node, filter, work_stack, &mut emit_event, &mut add_watch);
                }
                // skip any pending changes for child directories
                while transaction
                    .next_if(|next_change| change.path.is_parent_of(&next_change.path))
                    .is_some()
                {}
            }
        }
    }

    fn reserve_dir(&mut self, node: NodeId, size: usize) -> DirId {
        let dir = self.dirs.len().into();
        self[node].children = dir;
        self.dirs.push(EcoVec::with_capacity(size));
        dir
    }

    fn add_child(&mut self, node: NodeId, child: NodeId) {
        let dir = if self[node].children.is_none() {
            self.reserve_dir(node, 4)
        } else {
            self[node].children
        };
        self.dirs[dir.idx()].push(child);
    }

    /// Applies a change to the in-memory file tree
    pub fn apply_change(
        &mut self,
        change: &PendingChange,
        work_stack: &mut Vec<(NodeId, usize)>,
        mut emit_event: impl FnMut(CanonicalPathBuf, EventType),
    ) -> (NodeId, bool) {
        let fs_meta = Metadata::for_path(&change.path);

        let hash = self.hasher.hash_one(&change.path);
        let entry = self.path_table.entry(
            hash,
            |&tree_id| self.nodes[tree_id.idx()].path == change.path,
            |id| self.hasher.hash_one(&self.nodes[id.idx()].path),
        );
        // A NEEDS_NON_RECURSIVE_CRAWL change (FSEvents reports *that* a directory changed,
        // not *what*) must also trigger a (re)crawl of that directory -- otherwise the change
        // inside it is never detected. The crawl's depth is still decided by the node's own
        // RECURSIVE flag. (inotify never sets this flag, so the Linux path is unaffected.)
        let mut recursive = change.flags.intersects(
            pending::Flags::NEEDS_RECURSIVE_CRAWL | pending::Flags::NEEDS_NON_RECURSIVE_CRAWL,
        );
        let mark_recursive = change.flags.contains(pending::Flags::MARK_RECURSIVE);
        match entry {
            Entry::Occupied(entry) => {
                let id = *entry.get();
                let node = &mut self.nodes[id.idx()];
                if mark_recursive {
                    node.flags |= Flags::RECURSIVE
                }
                if let Some(fs_meta) = fs_meta {
                    let meta = NodeMeta::new(&fs_meta);
                    let inode_changed = fs_meta.inode != node.inode;
                    // If the inode number changed then we definitely need to recursively
                    // examine any children because we cannot assume that the kernel will
                    // have given us the correct hints about this change.  BTRFS is one
                    // example of a filesystem where this has been observed to happen.
                    recursive |= inode_changed;
                    node.inode = fs_meta.inode;
                    let changed = node.meta.change_type(
                        &meta,
                        inode_changed | change.flags.contains(pending::Flags::ORIGIN_WATCHER),
                    );
                    if let Some(changed) = changed {
                        emit_event(change.path.clone(), changed);
                        recursive |= changed == EventType::Create;
                    }
                    node.meta = meta;
                    let watch_children = node.flags.contains(Flags::WATCH_CHILDREN);
                    if fs_meta.is_dir
                        && node.children.is_none()
                        && fs_meta.size != 0
                        && watch_children
                    {
                        self.reserve_dir(id, fs_meta.size);
                    }
                    (id, recursive && watch_children)
                } else {
                    let old_meta = replace(&mut node.meta, NodeMeta::Deleted);
                    match old_meta {
                        NodeMeta::Dir => self.delete_rec(id, work_stack, &mut emit_event),
                        NodeMeta::File { .. } => emit_event(change.path.clone(), EventType::Delete),
                        NodeMeta::Deleted => (),
                    }
                    (id, true)
                }
            }
            Entry::Vacant(entry) => {
                let Some(fs_meta) = fs_meta else {
                    return (NodeId::NONE, true);
                };
                let meta = NodeMeta::new(&fs_meta);
                let id = NodeId::from(self.nodes.len());
                entry.insert(id);
                let parent = change.path.parent().and_then(|parent| {
                    let hash = self.hasher.hash_one(parent.as_os_str());
                    self.path_table
                        .find(hash, |&id| self.nodes[id.idx()].path == parent)
                        .copied()
                });
                let Some(parent) = parent else {
                    log::error!("for {change:?} the parent wasn't yet in the tree! Ignoring...");
                    // remove inserted entry again as insertion failed
                    self.path_table
                        .find_entry(hash, |&tree_id| tree_id == id)
                        .unwrap()
                        .remove();
                    return (NodeId::NONE, true);
                };
                self.add_child(parent, id);
                recursive = mark_recursive || self[parent].flags.contains(Flags::RECURSIVE);
                let flags = if recursive {
                    Flags::RECURSIVE
                } else {
                    Flags::empty()
                };
                self.nodes.push(FsNode {
                    path: change.path.clone(),
                    meta,
                    flags,
                    inode: fs_meta.inode,
                    children: DirId::NONE,
                });
                if !fs_meta.is_dir {
                    emit_event(change.path.clone(), EventType::Create)
                } else if recursive && fs_meta.size != 0 {
                    self.reserve_dir(id, fs_meta.size);
                }
                (id, recursive)
            }
        }
    }

    pub fn add_root(&mut self, root: CanonicalPathBuf, recursive: bool) -> Option<NodeId> {
        self.add(root, recursive, true)
    }

    fn add(&mut self, path: CanonicalPathBuf, recursive: bool, root: bool) -> Option<NodeId> {
        let hash = self.hasher.hash_one(&path);
        let entry = self.path_table.entry(
            hash,
            |&tree_id| self.nodes[tree_id.idx()].path == path,
            |id| self.hasher.hash_one(&self.nodes[id.idx()].path),
        );
        match entry {
            Entry::Occupied(entry) => {
                // we only want to add new entries here but if we are a recursive watch
                // and the target is not being recursively watched then we still have to add it
                if !recursive {
                    log::error!("already watching {path:?}");
                    return None;
                }
                let id = *entry.get();
                // also roots can only be dirs, not files
                if root && !self[id].meta.is_dir() {
                    log::error!("invalid root {path:?}: not a directory");
                    return None;
                }
                if self[id].flags.contains(Flags::RECURSIVE) {
                    log::error!("already watching recursively {path:?}");
                    return None;
                }
                self[id].flags.insert(Flags::RECURSIVE);
                Some(id)
            }
            Entry::Vacant(entry) => {
                let fs_meta = Metadata::for_path(&path)?;
                let meta = NodeMeta::new(&fs_meta);
                let id = NodeId::from(self.nodes.len());
                entry.insert(id);
                let parent = path.parent().and_then(|parent| {
                    let hash = self.hasher.hash_one(parent.as_os_str());
                    self.path_table
                        .find(hash, |&id| self.nodes[id.idx()].path == parent)
                        .copied()
                });
                if let Some(parent) = parent {
                    self.add_child(parent, id);
                } else if !root {
                    log::error!("for {path:?} the parent wasn't yet in the tree! Ignoring...");
                    // Use id comparison instead of accessing self.nodes since the node
                    // hasn't been added yet (would cause index out of bounds)
                    self.path_table
                        .find_entry(hash, |&tree_id| tree_id == id)
                        .unwrap()
                        .remove();
                    return None;
                };
                self.nodes.push(FsNode {
                    path: path.clone(),
                    meta,
                    inode: fs_meta.inode,
                    children: DirId::NONE,
                    flags: if recursive {
                        Flags::RECURSIVE
                    } else if root {
                        Flags::WATCH_CHILDREN
                    } else {
                        Flags::empty()
                    },
                });
                if fs_meta.is_dir && (recursive || root) && fs_meta.size != 0 {
                    self.reserve_dir(id, fs_meta.size);
                }
                Some(id)
            }
        }
    }

    /// recursively marks any children of the give filesystem node
    /// as deleted
    fn delete_rec(
        &mut self,
        id: NodeId,
        work_stack: &mut Vec<(NodeId, usize)>,
        mut emit_event: impl FnMut(CanonicalPathBuf, EventType),
    ) {
        if self[id].children.is_none() {
            return;
        }
        self[id].meta = NodeMeta::Deleted;
        let start_len = work_stack.len();
        work_stack.push((id, 0));
        while work_stack.len() > start_len {
            let (id, child) = work_stack.last_mut().unwrap();
            let Some(&child_id) = self[self[*id].children].get(*child) else {
                self[*id].meta = NodeMeta::Deleted;
                work_stack.pop();
                continue;
            };
            *child += 1;
            if self[child_id].meta.is_file() {
                emit_event(self[child_id].path.clone(), EventType::Delete);
            } else if self[child_id].meta.is_dir() && self[child_id].children.is_some() {
                work_stack.push((child_id, 0));
            }
            self[child_id].meta = NodeMeta::Deleted
        }
    }

    // (recursively) crawl a directory to re-synchronize the file tree
    // and record any changes observed along the way
    pub fn crawl(
        &mut self,
        root: NodeId,
        filter: &dyn Filter,
        work_stack: &mut Vec<(NodeId, usize)>,
        mut emit_event: impl FnMut(CanonicalPathBuf, EventType),
        mut add_watch: impl FnMut(CanonicalPathBuf),
    ) {
        let mut walk_builder = WalkDir::new(self[root].path.as_std_path())
            .follow_links(false)
            .follow_root_links(false)
            .same_file_system(true);
        let recursive = self[root].flags.contains(Flags::RECURSIVE);
        let flags = if recursive {
            pending::Flags::NEEDS_RECURSIVE_CRAWL | pending::Flags::MARK_RECURSIVE
        } else {
            walk_builder = walk_builder.max_depth(1);
            pending::Flags::NEEDS_RECURSIVE_CRAWL
        };
        add_watch(self[root].path.clone());
        if self[root].children.is_some() {
            for &child in &self.dirs[self[root].children.idx()] {
                self.nodes[child.idx()].set_maybe_deleted_flag();
            }
            work_stack.push((root, 0));
        }

        let mut walk = walk_builder.into_iter();
        while let Some(child) = walk.next() {
            let Ok(child) = child else {
                // TODO: why can this fail? permission issue?
                // how to handle that? just ignore?
                continue;
            };
            // the root was already analyzed by the caller don't restart it
            if child.depth() == 0 {
                continue;
            }
            if filter.ignore_path(child.path(), Some(child.file_type().is_dir())) {
                if child.file_type().is_dir() {
                    walk.skip_current_dir()
                }
                continue;
            }
            let path = CanonicalPathBuf::assert_canonicalized(child.path());
            let change = PendingChange { path, flags };
            let (node, _) = self.apply_change(&change, work_stack, &mut emit_event);

            // apply_change can return NodeId::NONE if the path doesn't exist
            // (e.g., deleted between walkdir iteration and stat)
            if node.is_none() {
                continue;
            }
            self[node].unset_maybe_deleted_flag();
            while work_stack
                .last()
                .is_some_and(|(_, depth)| *depth >= child.depth())
            {
                let (node, _) = work_stack.pop().unwrap();
                for &child in &self.dirs[self[node].children.idx()].clone() {
                    if self.nodes[child.idx()].maybe_deleted_flag() {
                        emit_event(self[child].path.clone(), EventType::Delete);
                        self.delete_rec(child, work_stack, &mut emit_event);
                    }
                }
            }
            if self[node].meta.is_dir() && recursive {
                add_watch(change.path.clone());
                // track which directories we are entering/exiting so that we can mark any
                // files that were not visited as removed
                if self[node].children.is_some() {
                    for &child in &self.dirs[self[node].children.idx()] {
                        self.nodes[child.idx()].set_maybe_deleted_flag();
                    }
                    work_stack.push((node, child.depth()));
                }
            }
        }
        while let Some((node, _)) = work_stack.pop() {
            for &child in &self.dirs[self[node].children.idx()].clone() {
                if self.nodes[child.idx()].maybe_deleted_flag() {
                    emit_event(self[child].path.clone(), EventType::Delete);
                    self.delete_rec(child, work_stack, &mut emit_event);
                }
            }
        }
    }

    pub fn crawl_root(
        &mut self,
        root: NodeId,
        recursive: bool,
        filter: &dyn Filter,
        mut add_watch: impl FnMut(CanonicalPathBuf),
    ) {
        let mut walk = WalkDir::new(self[root].path.as_std_path())
            .follow_links(false)
            .follow_root_links(false)
            .same_file_system(true);
        if !recursive {
            walk = walk.max_depth(1);
        }
        let mut walk = walk.into_iter();
        while let Some(child) = walk.next() {
            let Ok(child) = child else {
                // TODO: why can this fail? permission issue?
                // how to handle that? just ignore?
                continue;
            };
            if child.depth() == 0 {
                continue;
            }
            if filter.ignore_path(child.path(), Some(child.file_type().is_dir())) {
                if child.file_type().is_dir() {
                    walk.skip_current_dir()
                }
                continue;
            }
            let path = CanonicalPathBuf::assert_canonicalized(child.path());
            if let Some(node) = self.add(path.clone(), recursive, false) {
                if self[node].meta.is_dir() && recursive {
                    add_watch(self[node].path.clone())
                }
            } else {
                walk.skip_current_dir()
            }
        }
    }
}

impl Index<NodeId> for FileTree {
    type Output = FsNode;

    fn index(&self, index: NodeId) -> &Self::Output {
        &self.nodes[index.idx()]
    }
}

impl IndexMut<NodeId> for FileTree {
    fn index_mut(&mut self, index: NodeId) -> &mut Self::Output {
        &mut self.nodes[index.idx()]
    }
}

impl Index<DirId> for FileTree {
    type Output = EcoVec<NodeId>;

    fn index(&self, index: DirId) -> &Self::Output {
        &self.dirs[index.idx()]
    }
}

impl IndexMut<DirId> for FileTree {
    fn index_mut(&mut self, index: DirId) -> &mut Self::Output {
        &mut self.dirs[index.idx()]
    }
}

#[cfg(test)]
mod tests {
    use super::{DirId, NodeId};

    /// FSEvents reports "directory X changed" via `NEEDS_NON_RECURSIVE_CRAWL`; the tree must
    /// re-crawl X and surface the change inside it. This flag used to be emitted but never
    /// consumed, so macOS detected no in-directory changes at all.
    #[test]
    fn non_recursive_crawl_detects_inner_change() {
        use super::FileTree;
        use crate::events::EventType;
        use crate::path::CanonicalPathBuf;
        use crate::pending::{Flags, PendingChanges};
        use std::fs;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        let file = sub.join("f");
        fs::write(&file, b"v1").unwrap();

        let mut tree = FileTree::new();
        let root = tree
            .add_root(CanonicalPathBuf::assert_canonicalized(dir.path()), true)
            .unwrap();
        tree.crawl_root(root, true, &(), |_| {});

        // Change the file so a re-crawl of `sub` would notice it (different size/mtime).
        fs::write(&file, b"v2 is longer").unwrap();

        let mut changes = PendingChanges::default();
        changes.add_watcher(
            CanonicalPathBuf::assert_canonicalized(&sub),
            Flags::NEEDS_NON_RECURSIVE_CRAWL,
        );
        let mut events = Vec::new();
        let mut work_stack = Vec::new();
        tree.apply_transaction(
            &mut changes,
            &(),
            |path, ty| events.push((path, ty)),
            &mut work_stack,
            |_| {},
        );
        assert!(
            events
                .iter()
                .any(|(p, ty)| *ty == EventType::Modified && p.as_std_path().ends_with("f")),
            "expected a Modified event for the inner file, got {events:?}",
        );
    }

    #[test]
    #[should_panic(expected = "NodeId::NONE used as a tree index")]
    fn node_none_index_panics_clearly() {
        let _ = NodeId::NONE.idx();
    }

    #[test]
    #[should_panic(expected = "DirId::NONE used as a tree index")]
    fn dir_none_index_panics_clearly() {
        let _ = DirId::NONE.idx();
    }

    #[test]
    fn valid_ids_index_normally() {
        assert_eq!(NodeId::from(7usize).idx(), 7);
        assert_eq!(DirId::from(3usize).idx(), 3);
    }
}
