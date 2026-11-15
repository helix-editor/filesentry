use std::path::Path;
#[cfg(not(watcher_disable))]
use std::sync::Arc;
#[cfg(not(watcher_disable))]
use std::time::Duration;

#[cfg(not(watcher_disable))]
use crate::events::Events;

#[cfg(not(watcher_disable))]
pub type Handler = Box<dyn FnMut(Events) -> bool + Send>;

#[cfg(not(watcher_disable))]
pub struct Config {
    pub(crate) filter: Arc<dyn Filter>,
    pub(crate) settle_time: Duration,
    pub(crate) handlers: Vec<Handler>,
}

#[cfg(not(watcher_disable))]
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("settle_time", &self.settle_time)
            .finish_non_exhaustive()
    }
}

pub trait Filter: 'static + Send + Sync {
    fn ignore_path_rec(&self, mut path: &Path, is_dir: Option<bool>) -> bool {
        // The leaf is checked with the caller-provided `is_dir`; every ancestor of a path is
        // necessarily a directory, so check those with `Some(true)` (filters with dir-only
        // patterns such as gitignore's `target/` depend on this).
        let mut is_dir = is_dir;
        loop {
            if self.ignore_path(path, is_dir) {
                return true;
            }
            let Some(parent) = path.parent() else {
                break;
            };
            path = parent;
            is_dir = Some(true);
        }
        false
    }
    fn ignore_path(&self, path: &Path, is_dir: Option<bool>) -> bool;
}

impl Filter for () {
    fn ignore_path(&self, path: &Path, _is_dir: Option<bool>) -> bool {
        path.ends_with(".git")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ignores a path component named `target`, but only when it is a directory.
    struct DirOnlyTarget;
    impl Filter for DirOnlyTarget {
        fn ignore_path(&self, path: &Path, is_dir: Option<bool>) -> bool {
            is_dir == Some(true) && path.file_name().is_some_and(|n| n == "target")
        }
    }

    #[test]
    fn ancestors_count_as_dirs() {
        let f = DirOnlyTarget;
        // The leaf is a file, but its ancestor `target` is a directory and must be
        // recognized as ignored even though the leaf is passed as `Some(false)`.
        assert!(f.ignore_path_rec(Path::new("/proj/target/foo.rs"), Some(false)));
        // No ignored-directory ancestor: not ignored.
        assert!(!f.ignore_path_rec(Path::new("/proj/src/foo.rs"), Some(false)));
    }
}
