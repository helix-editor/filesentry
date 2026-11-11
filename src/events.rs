use std::hash::BuildHasher;
use std::mem::replace;
use std::ops::Deref;

use ecow::EcoVec;
use hashbrown::{hash_table, DefaultHashBuilder, HashTable};

use crate::path::CanonicalPathBuf;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, PartialOrd, Ord)]
pub enum EventType {
    Create,
    Delete,
    Modified,
    /// Created and deleted again within one settle window; usually safe to ignore.
    Tempfile,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Event {
    pub path: CanonicalPathBuf,
    pub ty: EventType,
}

#[derive(Debug)]
pub(crate) struct EventDebouncer {
    table: HashTable<u32>,
    hasher: DefaultHashBuilder,
    events: EcoVec<Event>,
}

impl EventDebouncer {
    pub fn new() -> Self {
        Self {
            table: HashTable::with_capacity(128),
            hasher: DefaultHashBuilder::default(),
            events: EcoVec::with_capacity(8),
        }
    }

    pub fn add(&mut self, path: CanonicalPathBuf, ty: EventType) {
        let entry = self.table.entry(
            self.hasher.hash_one(&path),
            |&i| self.events[i as usize].path == path,
            |&i| self.hasher.hash_one(&self.events[i as usize].path),
        );
        match entry {
            hash_table::Entry::Occupied(entry) => {
                let i = *entry.get() as usize;
                let event = &mut self.events.make_mut()[i];
                match (event.ty, ty) {
                    (EventType::Create, EventType::Delete) => event.ty = EventType::Tempfile,
                    (_, EventType::Delete) => {
                        event.ty = EventType::Delete;
                    }
                    (EventType::Delete, EventType::Create) => {
                        event.ty = EventType::Modified;
                    }
                    // it exists again: a `Tempfile` would be ignored
                    (EventType::Tempfile, EventType::Create) => {
                        event.ty = EventType::Create;
                    }
                    (EventType::Create, EventType::Modified)
                    | (EventType::Modified, EventType::Modified) => (),
                    (old, new) => {
                        log::error!(
                            "cannot merge {old:?}->{new:?} for {path}, this should be impossible!",
                        )
                    }
                }
            }
            hash_table::Entry::Vacant(entry) => {
                entry.insert(self.events.len() as u32);
                self.events.push(Event { path, ty });
            }
        }
    }

    pub fn take(&mut self) -> Events {
        self.table.clear();
        Events {
            events: replace(&mut self.events, EcoVec::with_capacity(8)),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The coalesced event currently held for `path`, if any.
    #[cfg(test)]
    pub(crate) fn get(&self, path: &CanonicalPathBuf) -> Option<EventType> {
        self.table
            .find(self.hasher.hash_one(path), |&i| {
                self.events[i as usize].path == *path
            })
            .map(|&i| self.events[i as usize].ty)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Events {
    events: EcoVec<Event>,
}

impl Deref for Events {
    type Target = [Event];

    fn deref(&self) -> &Self::Target {
        &self.events
    }
}

impl From<Vec<Event>> for Events {
    fn from(events: Vec<Event>) -> Self {
        Self {
            events: events.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::CanonicalPathBuf;
    use std::path::Path;

    fn path(p: &str) -> CanonicalPathBuf {
        CanonicalPathBuf::assert_canonicalized(Path::new(p))
    }

    #[test]
    fn recreated_tempfile_is_a_create() {
        let mut debouncer = EventDebouncer::new();
        debouncer.add(path("/tmp/x"), EventType::Create);
        debouncer.add(path("/tmp/x"), EventType::Delete);
        debouncer.add(path("/tmp/x"), EventType::Create);
        let events = debouncer.take();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ty, EventType::Create);
    }
}
