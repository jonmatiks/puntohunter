//! The extranonce-prefix allocator: 4 bytes unique among live connections and channels, shared by V1 and
//! SV2.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use super::coinbase::EXTRANONCE_PREFIX_LEN;

#[derive(Default)]
pub struct ExtranonceAllocator {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    next: u32,
    live: HashSet<u32>,
}

/// A prefix held by one connection or channel, released when dropped.
pub struct ExtranoncePrefix {
    value: u32,
    allocator: Arc<ExtranonceAllocator>,
}

impl ExtranonceAllocator {
    pub fn allocate(self: &Arc<Self>) -> ExtranoncePrefix {
        let mut state = self.state.lock().unwrap();
        // Counting on from the last prefix handed out keeps a released prefix from returning at once.
        let mut value = state.next;
        while !state.live.insert(value) {
            value = value.wrapping_add(1);
        }
        state.next = value.wrapping_add(1);
        ExtranoncePrefix {
            value,
            allocator: self.clone(),
        }
    }
}

impl ExtranoncePrefix {
    pub fn bytes(&self) -> [u8; EXTRANONCE_PREFIX_LEN] {
        self.value.to_be_bytes()
    }
}

impl Drop for ExtranoncePrefix {
    fn drop(&mut self) {
        self.allocator
            .state
            .lock()
            .unwrap()
            .live
            .remove(&self.value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_prefixes_are_unique_and_released_on_drop() {
        let allocator = Arc::new(ExtranonceAllocator::default());
        let held: Vec<_> = (0..1000).map(|_| allocator.allocate()).collect();
        let distinct: HashSet<_> = held.iter().map(ExtranoncePrefix::bytes).collect();
        assert_eq!(distinct.len(), held.len());

        drop(held);
        assert!(allocator.state.lock().unwrap().live.is_empty());
    }

    #[test]
    fn skips_a_prefix_still_live_after_wrapping() {
        let allocator = Arc::new(ExtranonceAllocator::default());
        let first = allocator.allocate();
        allocator.state.lock().unwrap().next = first.value;
        assert_ne!(allocator.allocate().bytes(), first.bytes());
    }
}
