//! Randomized oracle: drive the tree with backend-shaped notifications for random
//! filesystem operation chains. After every settle window, a mirror built only
//! from the delivered events must match the files on disk, every event's type
//! must agree with the before/after state of its path, and the live tree must
//! match an independent `read_dir` walk of the watched roots.
//!
//! `FILESENTRY_SWEEP_SEED` sets the first seed and `FILESENTRY_SWEEP_SEEDS` the
//! number of seeds.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use super::{FileTree, NodeMeta};
use crate::config::Filter;
use crate::events::{EventDebouncer, EventType};
use crate::path::CanonicalPathBuf;
use crate::pending::PendingChanges;

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn pick(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

thread_local! {
    /// Toggled during a run, like a config reload that calls `set_filter` with a
    /// recrawl.
    static IGNORE_D1: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Ignores every entry named `ign`, and `d1` while `IGNORE_D1` is set.
struct Ignore;

impl Filter for Ignore {
    fn ignore_path(&self, path: &Path, _is_dir: Option<bool>) -> bool {
        path.file_name()
            .is_some_and(|name| name == "ign" || (name == "d1" && IGNORE_D1.get()))
    }
}

fn canon(path: &Path) -> CanonicalPathBuf {
    CanonicalPathBuf::assert_canonicalized(path)
}

const DIRS: &[&str] = &[
    "d0", "d1", "d0/d2", "d0/d2/d3", "ign", "ign/n", "ign/n/d4", "d1/ign",
];
const FILES: &[&str] = &[
    "f0",
    "f1",
    "d0/f2",
    "d0/d2/f3",
    "d0/d2/d3/f4",
    "d1/f5",
    "ign/f6",
    "ign/n/f7",
    "ign/n/d4/f8",
    "d1/ign/f9",
];
const LINKS: &[&str] = &["s0", "d0/s1"];

#[derive(Clone, Copy, PartialEq)]
enum Backend {
    /// inotify/ReadDirectoryChangesW: one notification per changed entry, only
    /// from directories that are actually watched.
    Precise,
    /// FSEvents: only "something changed in this directory".
    DirGranular,
}

/// The tree plus the set of directories it asked the backend to watch.
struct Harness {
    tree: FileTree,
    watched: BTreeSet<PathBuf>,
}

impl Harness {
    fn add_root(&mut self, path: &Path, debouncer: &mut EventDebouncer) {
        let path = canon(path);
        self.watched.insert(path.as_std_path().to_owned());
        let watched = &mut self.watched;
        self.tree.track_root(
            path,
            true,
            &Ignore,
            |p, ty| debouncer.add(p, ty),
            |p| {
                watched.insert(p.as_std_path().to_owned());
            },
        );
    }

    fn process(&mut self, pending: &mut PendingChanges, debouncer: &mut EventDebouncer) {
        let watched = &mut self.watched;
        let mut add_watch = |p: CanonicalPathBuf| {
            watched.insert(p.as_std_path().to_owned());
        };
        if pending.take_recrawl() {
            self.tree
                .recrawl(&Ignore, |p, ty| debouncer.add(p, ty), &mut add_watch);
        } else {
            self.tree.apply_transaction(
                pending,
                &Ignore,
                |p, ty| debouncer.add(p, ty),
                &mut add_watch,
            );
        }
    }

    fn live_nodes(&self) -> BTreeSet<(PathBuf, bool)> {
        self.tree
            .nodes
            .iter()
            .filter(|n| n.meta != NodeMeta::Deleted)
            .map(|n| (n.path.as_std_path().to_owned(), n.meta.is_dir()))
            .collect()
    }
}

/// What the tree should hold for `roots`, computed straight from the disk.
fn reference(roots: &[PathBuf]) -> BTreeSet<(PathBuf, bool)> {
    let mut nodes = BTreeSet::new();
    for root in roots {
        nodes.insert((root.clone(), true));
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let ty = fs::symlink_metadata(&path).unwrap().file_type();
                if !(ty.is_file() || ty.is_dir()) || Ignore.ignore_path(&path, Some(ty.is_dir())) {
                    continue;
                }
                if ty.is_dir() {
                    stack.push(path.clone());
                }
                nodes.insert((path, ty.is_dir()));
            }
        }
    }
    nodes
}

fn files(nodes: &BTreeSet<(PathBuf, bool)>) -> BTreeSet<PathBuf> {
    nodes
        .iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(p, _)| p.clone())
        .collect()
}

fn diff<T: Ord + std::fmt::Debug>(actual: &BTreeSet<T>, expected: &BTreeSet<T>) -> String {
    format!(
        "extra {:?}, missing {:?}",
        actual.difference(expected).collect::<Vec<_>>(),
        expected.difference(actual).collect::<Vec<_>>()
    )
}

/// Every path under (and including) `path`, parents first.
fn subtree(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_owned()];
    let mut i = 0;
    while i < out.len() {
        let p = out[i].clone();
        if p.is_dir() && !p.is_symlink() {
            for entry in fs::read_dir(&p).unwrap() {
                out.push(entry.unwrap().path());
            }
        }
        i += 1;
    }
    out
}

/// Turns filesystem operations into the notifications a backend would queue.
struct Notifier<'a> {
    backend: Backend,
    root: &'a Path,
    /// The roots an FSEvents stream would cover.
    stream: &'a [PathBuf],
    h: &'a mut Harness,
    pending: &'a mut PendingChanges,
    touched: &'a mut BTreeSet<PathBuf>,
    /// Off while setting up the initial disk state, before any root is watched.
    live: bool,
}

impl Notifier<'_> {
    /// Queues a change of `path`. `crawl` is set for a create or delete, not for a
    /// modification or half of a rename.
    fn changed(&mut self, path: &Path, crawl: bool) {
        self.touched.insert(path.to_owned());
        if !self.live {
            return;
        }
        let parent = path.parent().unwrap();
        match self.backend {
            Backend::Precise => {
                if self.h.watched.contains(parent) && !Ignore.ignore_path(path, None) {
                    self.pending.add(canon(path), crawl);
                }
            }
            Backend::DirGranular => {
                if self.stream.iter().any(|r| parent.starts_with(r))
                    && !Ignore.ignore_path_rec(parent, Some(true))
                {
                    self.pending.add(canon(parent), true);
                }
            }
        }
    }

    /// `paths` (a subtree, parents first) went away: each watched directory
    /// reports its entries, then its own removal (DELETE_SELF/IGNORED).
    fn removed(&mut self, paths: &[PathBuf]) {
        for p in paths {
            self.changed(p, true);
        }
        for p in paths {
            if self.h.watched.remove(p) && self.live && self.backend == Backend::Precise {
                self.pending.add(canon(p), true);
            }
        }
    }
}

fn op(rng: &mut Lcg, n: &mut Notifier, protected: &[PathBuf]) -> String {
    let root = n.root.to_owned();
    let pick = |rng: &mut Lcg, names: &[&str]| root.join(names[rng.pick(names.len())]);
    let is_protected = |p: &Path| protected.iter().any(|r| r.starts_with(p));
    match rng.pick(9) {
        // create or overwrite a file
        0 | 1 => {
            let f = pick(rng, FILES);
            if !f.parent().unwrap().is_dir() || f.is_dir() || f.is_symlink() {
                return "skip".into();
            }
            let existed = f.exists();
            fs::write(&f, format!("v{}", rng.next())).unwrap();
            n.changed(&f, !existed);
            format!("write {f:?}")
        }
        2 => {
            let d = pick(rng, DIRS);
            if d.exists() || d.is_symlink() || !d.parent().unwrap().is_dir() {
                return "skip".into();
            }
            fs::create_dir(&d).unwrap();
            n.changed(&d, true);
            format!("mkdir {d:?}")
        }
        // rm -r, optionally leaving a file in its place
        3 | 4 => {
            let d = pick(rng, DIRS);
            if !d.is_dir() || d.is_symlink() || is_protected(&d) {
                return "skip".into();
            }
            let paths = subtree(&d);
            fs::remove_dir_all(&d).unwrap();
            n.removed(&paths);
            if rng.pick(2) == 0 {
                fs::write(&d, b"now a file").unwrap();
                n.changed(&d, true);
                return format!("rm -r {d:?} && touch it");
            }
            format!("rm -r {d:?}")
        }
        5 => {
            let l = pick(rng, LINKS);
            if !l.parent().unwrap().is_dir() {
                return "skip".into();
            }
            if l.is_symlink() {
                fs::remove_file(&l).unwrap();
            } else if l.exists() {
                return "skip".into();
            } else {
                std::os::unix::fs::symlink("/nonexistent", &l).unwrap();
            }
            n.changed(&l, true);
            format!("toggle link {l:?}")
        }
        // rename a file (MOVED_FROM/MOVED_TO carry no create/delete flag)
        6 => {
            let from = pick(rng, FILES);
            let to = pick(rng, FILES);
            if !from.is_file() || from.is_symlink() || from == to {
                return "skip".into();
            }
            if to.is_dir() || to.is_symlink() || !to.parent().unwrap().is_dir() {
                return "skip".into();
            }
            fs::rename(&from, &to).unwrap();
            n.changed(&from, false);
            n.changed(&to, false);
            format!("mv {from:?} {to:?}")
        }
        // rename a directory to a fresh name
        7 => {
            let from = pick(rng, DIRS);
            let to = pick(rng, DIRS);
            if !from.is_dir() || from.is_symlink() || is_protected(&from) {
                return "skip".into();
            }
            if to.exists()
                || to.is_symlink()
                || !to.parent().unwrap().is_dir()
                || to.starts_with(&from)
            {
                return "skip".into();
            }
            let old = subtree(&from);
            fs::rename(&from, &to).unwrap();
            for p in &old {
                n.touched.insert(p.clone());
            }
            for p in subtree(&to) {
                n.touched.insert(p);
            }
            n.changed(&from, false);
            n.changed(&to, false);
            // The moved directory's own watch is dropped (MOVE_SELF), its
            // subdirectories' watches go stale; either way none is valid by path.
            for p in &old {
                if n.h.watched.remove(p) && n.live && n.backend == Backend::Precise && p == &from {
                    n.pending.add(canon(p), true);
                }
            }
            format!("mv -T {from:?} {to:?}")
        }
        8 => {
            if rng.pick(3) != 0 {
                return "noop".into();
            }
            IGNORE_D1.set(!IGNORE_D1.get());
            n.pending.recrawl();
            for p in subtree(&root.join("d1")) {
                n.touched.insert(p);
            }
            format!("ignore d1: {}", IGNORE_D1.get())
        }
        _ => unreachable!(),
    }
}

#[test]
fn random_op_chains_match_disk() {
    let env = |name, default| {
        std::env::var(name)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    };
    let seed0: u64 = env("FILESENTRY_SWEEP_SEED", 1);
    let seeds: u64 = env("FILESENTRY_SWEEP_SEEDS", 200);
    for seed in seed0..seed0 + seeds {
        sweep_one(seed);
    }
}

fn sweep_one(seed: u64) {
    IGNORE_D1.set(false);
    let mut rng = Lcg(seed ^ 0x9e3779b97f4a7c15);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let backend = if rng.pick(3) == 0 {
        Backend::DirGranular
    } else {
        Backend::Precise
    };
    // 0: one recursive root; 1: an inner root, the enclosing one added later;
    // 2: a nested root inside an ignored directory, added later;
    // 3: one non-recursive root.
    let scenario = rng.pick(4);
    let (initial, late, mut protected) = match scenario {
        0 => (vec![root.clone()], None, vec![]),
        _ => {
            let inner = root.join("d0");
            (vec![inner.clone()], Some(root.clone()), vec![inner])
        }
    };
    for p in &protected {
        fs::create_dir_all(p).unwrap();
    }

    let mut h = Harness {
        tree: FileTree::new(),
        watched: BTreeSet::new(),
    };
    let mut pending = PendingChanges::default();
    let mut debouncer = EventDebouncer::new();
    let mut log: Vec<String> = Vec::new();
    let mut touched = BTreeSet::new();

    // Populate the disk before watching, so the initial crawl has work to do.
    {
        let mut n = Notifier {
            backend,
            root: &root,
            stream: &[],
            h: &mut h,
            pending: &mut pending,
            touched: &mut touched,
            live: false,
        };
        for _ in 0..12 {
            let l = op(&mut rng, &mut n, &protected);
            if l != "skip" {
                log.push(format!("setup: {l}"));
            }
        }
    }
    let mut roots = initial;
    for path in &roots {
        h.add_root(path, &mut debouncer);
    }
    // The initial crawl reports nothing: seed the mirror from the disk.
    let mut mirror = files(&reference(&roots));

    for step in 0..30 {
        let ctx = |log: &[String]| {
            format!(
                "seed {seed} backend {} scenario {scenario}\n{}",
                backend as u8,
                log.join("\n")
            )
        };
        if step == 10 {
            if let Some(late) = &late {
                roots.push(late.clone());
                h.add_root(late, &mut debouncer);
                // an inner root is now just a subdirectory of the enclosing one
                if scenario == 1 {
                    roots.remove(0);
                    protected.clear();
                }
                log.push(format!("step {step}: add root {late:?}"));
                mirror.extend(files(&reference(&roots)));
            }
        }
        touched.clear();
        let mut n = Notifier {
            backend,
            root: &root,
            stream: &roots,
            h: &mut h,
            pending: &mut pending,
            touched: &mut touched,
            live: true,
        };
        for _ in 0..rng.pick(4) + 1 {
            let l = op(&mut rng, &mut n, &protected);
            if l != "skip" {
                log.push(format!("step {step}: {l}"));
            }
            // the worker may apply a batch before the window settles
            if rng.pick(4) == 0 {
                n.h.process(n.pending, &mut debouncer);
                log.push(format!("step {step}: (apply)"));
            }
        }
        // the kernel queue occasionally overflows and the worker recrawls
        if rng.pick(8) == 0 {
            pending.recrawl();
            log.push(format!("step {step}: OVERFLOW"));
        }
        h.process(&mut pending, &mut debouncer);

        let expected = reference(&roots);
        let disk = files(&expected);
        for event in debouncer.take().iter() {
            let path = event.path.as_std_path().to_owned();
            let (before, after) = (mirror.contains(&path), disk.contains(&path));
            let consistent = match event.ty {
                EventType::Create => !before && after,
                EventType::Delete => before && !after,
                EventType::Modified => before && after,
                EventType::Tempfile => !before && !after,
            };
            assert!(
                consistent && touched.contains(&path),
                "{:?} for {path:?} (before: {before}, after: {after}, touched: {})\n{}",
                event.ty,
                touched.contains(&path),
                ctx(&log)
            );
            match event.ty {
                EventType::Create | EventType::Modified => mirror.insert(path),
                EventType::Delete | EventType::Tempfile => mirror.remove(&path),
            };
        }
        assert!(
            mirror == disk,
            "mirror diverged from disk: {}\n{}",
            diff(&mirror, &disk),
            ctx(&log)
        );
        let live = h.live_nodes();
        assert!(
            live == expected,
            "tree diverged from the disk: {}\n{}",
            diff(&live, &expected),
            ctx(&log)
        );
    }
}
