//! Collapse concurrent identical loads into a single execution.
//!
//! When N threads ask for the same key at once, exactly one runs the loader;
//! the rest block and receive a clone of the shared result. This keeps a burst
//! of identical `read`/`grep` calls from all hitting the filesystem.

use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

struct Call<V> {
    result: Mutex<Option<V>>,
    ready: Condvar,
}

/// A keyed single-flight coordinator. `V` must be cheap to clone (e.g. `Arc`).
pub struct SingleFlight<K, V> {
    in_flight: Mutex<HashMap<K, Arc<Call<V>>>>,
}

impl<K, V> Default for SingleFlight<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn default() -> Self {
        Self {
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `loader` for `key` unless an identical call is already running, in
    /// which case block until it finishes and return a clone of its result.
    pub fn run(&self, key: K, loader: impl FnOnce() -> V) -> V {
        let call = {
            let mut map = self.in_flight.lock();
            if let Some(existing) = map.get(&key) {
                let existing = Arc::clone(existing);
                drop(map);
                // Follower: wait for the leader to publish the result.
                let mut guard = existing.result.lock();
                while guard.is_none() {
                    existing.ready.wait(&mut guard);
                }
                return guard.as_ref().unwrap().clone();
            }
            let call = Arc::new(Call {
                result: Mutex::new(None),
                ready: Condvar::new(),
            });
            map.insert(key.clone(), Arc::clone(&call));
            call
        };

        // Leader: run the loader, publish, wake followers, and deregister.
        let value = loader();
        {
            let mut slot = call.result.lock();
            *slot = Some(value.clone());
        }
        call.ready.notify_all();
        self.in_flight.lock().remove(&key);
        value
    }
}
