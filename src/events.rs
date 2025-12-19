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
    /// a file that was added and removed again immedietly
    /// (within the settle period) can usually be ignored
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
                    // temporary file that was created and immidiately removed
                    (EventType::Create, EventType::Delete) => event.ty = EventType::Tempfile,
                    (_, EventType::Delete) => {
                        event.ty = EventType::Delete;
                    }
                    (EventType::Delete, EventType::Create) => {
                        event.ty = EventType::Modified;
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
