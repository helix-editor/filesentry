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
        loop {
            if self.ignore_path(path, is_dir) {
                return true;
            }
            let Some(parent) = path.parent() else {
                break;
            };
            path = parent;
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
