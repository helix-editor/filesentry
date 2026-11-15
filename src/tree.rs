use std::ffi::OsStr;
use std::mem::replace;
use std::ops::{Index, IndexMut};
use std::time::SystemTime;

use bitflags::bitflags;
use ecow::EcoVec;
use hashbrown::{Equivalent, HashMap};
use walkdir::WalkDir;

use crate::config::Filter;
use crate::events::EventType;
use crate::metadata::Metadata;
use crate::path::CanonicalPathBuf;
use crate::pending::{PendingChange, PendingChanges};

type Emit<'a> = &'a mut dyn FnMut(CanonicalPathBuf, EventType);

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeMeta {
    Dir,
    File {
        mtime: SystemTime,
        size: usize,
    },
    /// Also the state of a node that was never seen on disk yet.
    Deleted,
}

impl NodeMeta {
    fn new(meta: &Metadata) -> NodeMeta {
        if meta.is_dir {
            NodeMeta::Dir
        } else {
            NodeMeta::File {
                mtime: meta.mtime,
                size: meta.size,
            }
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self, NodeMeta::Dir)
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
struct DirId(u32);

impl DirId {
    const NONE: DirId = DirId(u32::MAX);

    fn idx(self) -> usize {
        // fail here rather than as a baffling out-of-bounds index in release
        assert!(
            self != Self::NONE,
            "filesentry bug: DirId::NONE used as a tree index"
        );
        self.0 as usize
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub struct NodeId(u32);

impl NodeId {
    fn idx(self) -> usize {
        self.0 as usize
    }
}

const _: () = assert!(
    size_of::<FsNode>() == 7 * 8,
    "size of FsNode must stay constant"
);

bitflags! {
    #[derive(Clone, Copy, Debug)]
    struct Flags: u32 {
        /// Set on a directory's children when a crawl enters it. A child that is
        /// still marked when the crawl leaves the directory has been deleted.
        const MAYBE_DELETED = 1;
        /// Track this directory's children.
        const WATCH_CHILDREN = 0b10;
        /// Track this directory's whole subtree.
        const RECURSIVE = 0b110;
        /// Added as a root. It is tracked whether or not its parent is, and may have
        /// been inserted before its parent.
        const ROOT = 0b1000;
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

/// The last known state of the watched paths. Changed paths are stat'ed again
/// and diffed against it, which yields the file events.
pub struct FileTree {
    /// The keys share their buffers with the nodes' paths.
    by_path: HashMap<CanonicalPathBuf, NodeId>,
    nodes: Vec<FsNode>,
    dirs: Vec<EcoVec<NodeId>>,
    /// Every root added, in tree order, for a full resync.
    roots: Vec<NodeId>,
}

impl FileTree {
    pub fn new() -> Self {
        Self {
            by_path: HashMap::with_capacity(1024),
            nodes: Vec::with_capacity(1024),
            dirs: Vec::with_capacity(128),
            roots: Vec::new(),
        }
    }

    fn find(&self, path: &OsStr) -> Option<NodeId> {
        self.by_path.get(path).copied()
    }

    /// Adds a `Deleted` node for `path`, for `update` to fill in.
    fn insert(&mut self, path: CanonicalPathBuf, parent: Option<NodeId>, flags: Flags) -> NodeId {
        let id = NodeId(u32::try_from(self.nodes.len()).expect("too many nodes"));
        self.nodes.push(FsNode {
            path,
            meta: NodeMeta::Deleted,
            inode: 0,
            flags,
            children: DirId::NONE,
        });
        self.by_path.insert(self.nodes[id.idx()].path.clone(), id);
        if let Some(parent) = parent {
            self.add_child(parent, id);
        }
        id
    }

    fn add_child(&mut self, dir: NodeId, child: NodeId) {
        if self[dir].children == DirId::NONE {
            self[dir].children = DirId(self.dirs.len() as u32);
            self.dirs.push(EcoVec::new());
        }
        let children = self[dir].children;
        self.dirs[children.idx()].push(child);
    }

    /// A cheap copy, so the tree can be modified while iterating.
    fn children(&self, dir: NodeId) -> EcoVec<NodeId> {
        match self[dir].children {
            DirId::NONE => EcoVec::new(),
            children => self.dirs[children.idx()].clone(),
        }
    }

    /// Starts tracking the directory `path` as a root and crawls it, unless the
    /// tree already tracks it like that. Returns the root, or `None` if `path` is
    /// not a directory.
    pub fn track_root(
        &mut self,
        path: CanonicalPathBuf,
        recursive: bool,
        filter: &dyn Filter,
        emit: impl FnMut(CanonicalPathBuf, EventType),
        add_watch: impl FnMut(CanonicalPathBuf),
    ) -> Option<NodeId> {
        let watch = if recursive {
            Flags::RECURSIVE
        } else {
            Flags::WATCH_CHILDREN
        };
        let (id, new) = match self.find(path.as_os_str()) {
            Some(id) if self[id].meta != NodeMeta::Deleted => (id, false),
            // A deleted node may be stale: its directory was recreated, and the
            // event saying so hasn't arrived yet.
            found => {
                let meta = Metadata::for_path(&path).filter(|meta| meta.is_dir)?;
                let id = found.unwrap_or_else(|| {
                    let parent = path.parent().and_then(|p| self.find(p.as_os_str()));
                    self.insert(path.clone(), parent, Flags::empty())
                });
                self.update(id, Some(&meta), false, &mut |_, _| {});
                (id, true)
            }
        };
        if !self[id].meta.is_dir() {
            return None;
        }
        // A directory already tracked through an enclosing root becomes a root as
        // well, so it stays tracked if a filter change makes that root skip it.
        let crawl = new || !self[id].flags.contains(watch);
        self[id].flags |= watch | Flags::ROOT;
        if crawl {
            self.crawl(id, true, filter, emit, add_watch);
        }
        if let Err(i) = self.roots.binary_search_by(|&r| self[r].path.cmp(&path)) {
            self.roots.insert(i, id);
        }
        Some(id)
    }

    /// Re-syncs every root with the disk, for when the backend lost track of what
    /// changed. Enclosing roots are crawled first, and the roots their crawls reach
    /// are skipped.
    pub fn recrawl(
        &mut self,
        filter: &dyn Filter,
        mut emit: impl FnMut(CanonicalPathBuf, EventType),
        mut add_watch: impl FnMut(CanonicalPathBuf),
    ) {
        let mut covered = Vec::new();
        for i in 0..self.roots.len() {
            let root = self.roots[i];
            if !covered.contains(&root) {
                covered.extend(self.crawl_impl(root, false, filter, &mut emit, &mut add_watch));
            }
        }
    }

    /// Applies a batch of changes reported by the backend.
    pub fn apply_transaction(
        &mut self,
        changes: &mut PendingChanges,
        filter: &dyn Filter,
        mut emit: impl FnMut(CanonicalPathBuf, EventType),
        mut add_watch: impl FnMut(CanonicalPathBuf),
    ) {
        let mut changes = changes.drain().peekable();
        while let Some(change) = changes.next() {
            if self.apply_change(&change, filter, &mut emit, &mut add_watch) {
                while let Some(next) = changes.next_if(|next| change.path.is_parent_of(&next.path))
                {
                    // The crawl already covered `next`, but its notification still
                    // applies, because a crawl only compares metadata and the file
                    // may have changed without a new size, inode or mtime.
                    let next = PendingChange {
                        crawl: false,
                        ..next
                    };
                    self.apply_change(&next, filter, &mut emit, &mut add_watch);
                }
            }
        }
    }

    /// Re-stats a changed path. Returns true if that also covered everything below
    /// it, because the path was crawled or deleted, or isn't tracked.
    fn apply_change(
        &mut self,
        change: &PendingChange,
        filter: &dyn Filter,
        emit: Emit,
        add_watch: &mut dyn FnMut(CanonicalPathBuf),
    ) -> bool {
        let meta = Metadata::for_path(&change.path);
        let found = self.find(change.path.as_os_str());
        let id = match found {
            Some(id)
                if self[id].meta != NodeMeta::Deleted || self[id].flags.contains(Flags::ROOT) =>
            {
                id
            }
            _ => {
                // Backends report paths the tree doesn't track, for example through
                // a watch that outlived a filter change. Only a live directory that
                // tracks its children can start tracking a new entry.
                let Some(meta) = &meta else {
                    return true;
                };
                let parent = change.path.parent().and_then(|p| self.find(p.as_os_str()));
                let Some(parent) = parent.filter(|&p| {
                    self[p].meta.is_dir() && self[p].flags.contains(Flags::WATCH_CHILDREN)
                }) else {
                    return true;
                };
                if filter.ignore_path(change.path.as_std_path(), Some(meta.is_dir)) {
                    return true;
                }
                found.unwrap_or_else(|| {
                    let recursive = self[parent].flags.contains(Flags::RECURSIVE);
                    let flags = if recursive {
                        Flags::RECURSIVE
                    } else {
                        Flags::empty()
                    };
                    self.insert(change.path.clone(), Some(parent), flags)
                })
            }
        };
        let crawl = self.update(id, meta.as_ref(), true, emit) || change.crawl;
        let node = &self[id];
        match node.meta {
            NodeMeta::Deleted => true,
            NodeMeta::Dir if crawl && node.flags.contains(Flags::WATCH_CHILDREN) => {
                if !filter.ignore_path(change.path.as_std_path(), Some(true)) {
                    self.crawl_impl(id, false, filter, emit, add_watch);
                }
                true
            }
            _ => false,
        }
    }

    /// Records `meta`, a fresh stat or `None` if the path is gone, for `id` and
    /// emits the matching file event. When `precise` is set the backend reported
    /// this path, so a file that looks unchanged is still `Modified`. Returns
    /// whether the node is a directory whose contents need a crawl.
    fn update(&mut self, id: NodeId, meta: Option<&Metadata>, precise: bool, emit: Emit) -> bool {
        let Some(meta) = meta else {
            self.delete(id, emit);
            return false;
        };
        let new = NodeMeta::new(meta);
        let node = &self[id];
        let was_dir = node.meta.is_dir();
        // A changed inode means a replaced path, whose contents the backend may
        // not have reported (seen on BTRFS).
        let inode_changed = node.inode != meta.inode;
        let event = match (&node.meta, &new) {
            (NodeMeta::File { .. }, NodeMeta::File { .. }) => {
                (precise || inode_changed || node.meta != new).then_some(EventType::Modified)
            }
            (_, NodeMeta::File { .. }) => Some(EventType::Create),
            (NodeMeta::File { .. }, _) => Some(EventType::Delete),
            _ => None,
        };
        if was_dir && !meta.is_dir {
            self.delete(id, emit);
        }
        let node = &mut self[id];
        node.meta = new;
        node.inode = meta.inode;
        if let Some(event) = event {
            emit(node.path.clone(), event);
        }
        meta.is_dir && (inode_changed || !was_dir)
    }

    /// Marks `id` and everything below it deleted, emitting a `Delete` per file. A
    /// root that is still a directory on disk is kept, because it is tracked whether
    /// or not an enclosing root covers it.
    fn delete(&mut self, id: NodeId, emit: Emit) {
        let mut stack = vec![id];
        while let Some(id) = stack.pop() {
            let node = &mut self.nodes[id.idx()];
            if node.flags.contains(Flags::ROOT)
                && Metadata::for_path(&node.path).is_some_and(|meta| meta.is_dir)
            {
                continue;
            }
            match replace(&mut node.meta, NodeMeta::Deleted) {
                NodeMeta::File { .. } => emit(node.path.clone(), EventType::Delete),
                NodeMeta::Dir if node.children != DirId::NONE => {
                    stack.extend_from_slice(&self.dirs[node.children.idx()])
                }
                _ => (),
            }
        }
    }

    /// Re-syncs the contents of `root` (its whole subtree if it is recursive) with
    /// the disk. `initial` silences the `Create`s for new paths in directories the
    /// tree wasn't watching yet. A new root's contents aren't news, but changes
    /// inside an earlier root it encloses are.
    pub fn crawl(
        &mut self,
        root: NodeId,
        initial: bool,
        filter: &dyn Filter,
        mut emit: impl FnMut(CanonicalPathBuf, EventType),
        mut add_watch: impl FnMut(CanonicalPathBuf),
    ) {
        self.crawl_impl(root, initial, filter, &mut emit, &mut add_watch);
    }

    /// Returns the other roots the crawl walked into.
    fn crawl_impl(
        &mut self,
        root: NodeId,
        initial: bool,
        filter: &dyn Filter,
        emit: Emit,
        add_watch: &mut dyn FnMut(CanonicalPathBuf),
    ) -> Vec<NodeId> {
        let recursive = self[root].flags.contains(Flags::RECURSIVE);
        let mut walk = WalkDir::new(self[root].path.as_std_path())
            .min_depth(1)
            .follow_links(false)
            .follow_root_links(false)
            .same_file_system(true);
        if !recursive {
            walk = walk.max_depth(1);
        }
        add_watch(self[root].path.clone());
        // the directories the walk is in, with their depth and whether their
        // new entries are silenced
        let mut open = vec![(root, 0, initial)];
        let mut reached = Vec::new();
        self.mark_children(root, true);
        let mut walk = walk.into_iter();
        while let Some(entry) = walk.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    log::warn!("failed to crawl: {err}");
                    let dir = err.path().and_then(|path| {
                        open.iter()
                            .rev()
                            .find(|&&(dir, ..)| path.starts_with(self[dir].path.as_std_path()))
                    });
                    if let Some(&(dir, ..)) = dir {
                        self.mark_children(dir, false);
                    }
                    continue;
                }
            };
            while open
                .last()
                .is_some_and(|&(_, depth, _)| depth >= entry.depth())
            {
                let (dir, _, _) = open.pop().unwrap();
                self.sweep_children(dir, emit);
            }
            let is_dir = entry.file_type().is_dir();
            if filter.ignore_path(entry.path(), Some(is_dir)) {
                if is_dir {
                    walk.skip_current_dir();
                }
                continue;
            }
            let (parent, _, quiet) = *open.last().unwrap();
            let path = CanonicalPathBuf::assert_canonicalized(entry.path());
            let meta = Metadata::for_path(&path);
            let id = match self.find(path.as_os_str()) {
                Some(id) => {
                    if self[id].flags.contains(Flags::ROOT) && !self.children(parent).contains(&id)
                    {
                        self.add_child(parent, id);
                    }
                    self[id].flags.remove(Flags::MAYBE_DELETED);
                    self.update(id, meta.as_ref(), false, emit);
                    id
                }
                // vanished since the readdir, or a symlink
                None if meta.is_none() => continue,
                None => {
                    let id = self.insert(path.clone(), Some(parent), Flags::empty());
                    let sink: Emit = if quiet { &mut |_, _| {} } else { &mut *emit };
                    self.update(id, meta.as_ref(), false, sink);
                    id
                }
            };
            let watched = self[id].flags.contains(Flags::WATCH_CHILDREN);
            if recursive {
                self[id].flags |= Flags::RECURSIVE;
            }
            // Only descend into directories pushed on `open`, so that its top stays
            // the parent of the next entry.
            if recursive && self[id].meta.is_dir() {
                if self[id].flags.contains(Flags::ROOT) {
                    reached.push(id);
                }
                add_watch(path);
                self.mark_children(id, true);
                open.push((id, entry.depth(), quiet && !watched));
            } else if is_dir {
                walk.skip_current_dir();
            }
        }
        while let Some((dir, _, _)) = open.pop() {
            self.sweep_children(dir, emit);
        }
        reached
    }

    fn mark_children(&mut self, dir: NodeId, maybe_deleted: bool) {
        for &child in &self.children(dir) {
            self[child].flags.set(Flags::MAYBE_DELETED, maybe_deleted);
        }
    }

    /// Deletes the children of `dir` the walk didn't come across.
    fn sweep_children(&mut self, dir: NodeId, emit: Emit) {
        for &child in &self.children(dir) {
            if self[child].flags.contains(Flags::MAYBE_DELETED) {
                self[child].flags.remove(Flags::MAYBE_DELETED);
                self.delete(child, emit);
            }
        }
    }
}

/// Lets `by_path` be queried with a borrowed path, which hashes the same bytes.
impl Equivalent<CanonicalPathBuf> for OsStr {
    fn equivalent(&self, key: &CanonicalPathBuf) -> bool {
        key.as_os_str() == self
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{FileTree, NodeId};
    use crate::config::Filter;
    use crate::events::EventType;
    use crate::path::CanonicalPathBuf;
    use crate::pending::PendingChanges;

    type Events = Vec<(CanonicalPathBuf, EventType)>;

    fn canon(path: &Path) -> CanonicalPathBuf {
        CanonicalPathBuf::assert_canonicalized(path)
    }

    /// A temporary directory and its canonical path, which is what the tree
    /// reports (macOS `/var` is `/private/var`, Windows paths get a `\\?\` prefix).
    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    fn watch(root: &Path, recursive: bool) -> (FileTree, NodeId) {
        let mut tree = FileTree::new();
        let id = tree
            .track_root(canon(root), recursive, &(), |_, _| {}, |_| {})
            .unwrap();
        (tree, id)
    }

    fn crawl(tree: &mut FileTree, root: NodeId, filter: &dyn Filter) -> Events {
        let mut events = Vec::new();
        tree.crawl(root, false, filter, |p, ty| events.push((p, ty)), |_| {});
        events
    }

    /// Applies one batch of backend notifications, given as `(path, crawl)` pairs.
    fn apply(tree: &mut FileTree, filter: &dyn Filter, changes: &[(&Path, bool)]) -> Events {
        let mut pending = PendingChanges::default();
        for &(path, crawl) in changes {
            pending.add(canon(path), crawl);
        }
        let mut events = Vec::new();
        tree.apply_transaction(&mut pending, filter, |p, ty| events.push((p, ty)), |_| {});
        events
    }

    fn has(events: &Events, ty: EventType, path: &Path) -> bool {
        events.iter().any(|(p, t)| *t == ty && *p == path)
    }

    /// A directory that was deleted and recreated stays a stale node until its own
    /// event arrives. Adding it as a root must still track it.
    #[test]
    fn recreated_dir_as_root() {
        let (_dir, root) = tempdir();
        let d = root.join("d");
        fs::create_dir(&d).unwrap();
        let (mut tree, id) = watch(&root, true);
        fs::remove_dir(&d).unwrap();
        crawl(&mut tree, id, &());
        fs::create_dir(&d).unwrap();
        let inner = tree
            .track_root(canon(&d), true, &(), |_, _| {}, |_| {})
            .expect("root not added");
        fs::write(d.join("f"), b"x").unwrap();
        let events = crawl(&mut tree, inner, &());
        assert!(has(&events, EventType::Create, &d.join("f")), "{events:?}");
    }

    /// A directory renamed away and back keeps its inode and gets no create
    /// event, but its node must still be crawled again.
    #[test]
    fn directory_renamed_back_is_recrawled() {
        let (_dir, root) = tempdir();
        let (d, x) = (root.join("d"), root.join("x"));
        fs::create_dir(&d).unwrap();
        fs::write(d.join("f"), b"v1").unwrap();
        let (mut tree, _) = watch(&root, true);
        fs::rename(&d, &x).unwrap();
        apply(&mut tree, &(), &[(&d, false), (&x, false)]);
        fs::rename(&x, &d).unwrap();
        let events = apply(&mut tree, &(), &[(&x, false), (&d, false)]);
        assert!(has(&events, EventType::Create, &d.join("f")), "{events:?}");
    }

    /// A crawl only compares metadata, so the crawl of an ancestor must not drop the
    /// notification for a file rewritten with the same size and mtime, whether or
    /// not the notification also asked for a crawl.
    #[test]
    fn crawl_keeps_precise_notification() {
        for file_crawl in [false, true] {
            let (_dir, root) = tempdir();
            let file = root.join("f");
            fs::write(&file, b"old").unwrap();
            let mtime = fs::metadata(&file).unwrap().modified().unwrap();
            let (mut tree, _) = watch(&root, true);
            fs::write(&file, b"new").unwrap();
            fs::File::options()
                .write(true)
                .open(&file)
                .unwrap()
                .set_modified(mtime)
                .unwrap();
            let events = apply(&mut tree, &(), &[(&root, true), (&file, file_crawl)]);
            assert!(has(&events, EventType::Modified, &file), "{events:?}");
        }
    }

    /// The initial crawl of a root added around an earlier one only silences new
    /// paths in directories the tree didn't watch yet. A file created in the inner
    /// root before that crawl is still a `Create`.
    #[test]
    fn initial_crawl_keeps_inner_creates() {
        let (_dir, outer) = tempdir();
        let inner = outer.join("inner");
        fs::create_dir(&inner).unwrap();
        fs::write(outer.join("old"), b"x").unwrap();
        let (mut tree, _) = watch(&inner, true);
        let file = inner.join("new");
        fs::write(&file, b"x").unwrap();
        let mut events = Vec::new();
        tree.track_root(
            canon(&outer),
            true,
            &(),
            |p, ty| events.push((p, ty)),
            |_| {},
        );
        assert_eq!(events, [(canon(&file), EventType::Create)]);
    }

    /// A directory the crawl can't read isn't empty, so its files shouldn't be
    /// reported as deleted.
    #[test]
    #[cfg(unix)]
    fn unreadable_dir_keeps_its_files() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root) = tempdir();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("f"), b"x").unwrap();
        let (mut tree, id) = watch(&root, true);
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).unwrap();
        // the superuser can read it anyway
        let unreadable = fs::read_dir(&sub).is_err();
        let events = crawl(&mut tree, id, &());
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).unwrap();
        if unreadable {
            assert_eq!(events, Events::new());
        }
    }

    /// A root inside another stays tracked, in whichever order they were added,
    /// when a filter change makes the enclosing root skip the directory between
    /// them.
    #[test]
    fn covered_root_survives_filter_change() {
        struct IgnoreMid;
        impl Filter for IgnoreMid {
            fn ignore_path(&self, path: &Path, _: Option<bool>) -> bool {
                path.file_name().is_some_and(|n| n == "mid")
            }
        }
        for inner_first in [true, false] {
            let (_dir, outer) = tempdir();
            let inner = outer.join("mid/inner");
            fs::create_dir_all(&inner).unwrap();
            fs::write(inner.join("old"), b"x").unwrap();
            let mut tree = FileTree::new();
            let mut roots = [&inner, &outer];
            if !inner_first {
                roots.reverse();
            }
            for root in roots {
                tree.track_root(canon(root), true, &(), |_, _| {}, |_| {});
            }
            // the enclosing root's crawl covers the inner one
            let mut watched = Vec::new();
            tree.recrawl(&(), |_, _| {}, |p| watched.push(p));
            assert_eq!(watched.iter().filter(|p| **p == inner).count(), 1);
            let mut events = Vec::new();
            tree.recrawl(&IgnoreMid, |p, ty| events.push((p, ty)), |_| {});
            assert_eq!(events, Events::new());
            let file = inner.join("new");
            fs::write(&file, b"x").unwrap();
            let events = apply(&mut tree, &IgnoreMid, &[(&file, true)]);
            assert_eq!(events, [(canon(&file), EventType::Create)]);
        }
    }
}

#[cfg(all(test, unix))]
mod sweep;
