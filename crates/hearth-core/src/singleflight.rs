//! Collapse concurrent identical loads into a single execution.
//!
//! When N threads ask for the same key at once, exactly one runs the loader;
//! the rest block and receive a clone of the shared result. This keeps a burst
//! of identical `read`/`grep` calls from all hitting the filesystem.

use parking_lot::{Condvar, Mutex};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Arc;

enum CallState<V> {
    Running,
    Ready(V),
    Aborted,
}

struct Call<V> {
    state: Mutex<CallState<V>>,
    ready: Condvar,
}

impl<V> Call<V> {
    fn new() -> Self {
        Self {
            state: Mutex::new(CallState::Running),
            ready: Condvar::new(),
        }
    }
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
    K: Eq + Hash,
{
    fn remove_call(&self, key: &K, call: &Arc<Call<V>>) {
        let mut map = self.in_flight.lock();
        let is_current = map
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, call));
        if is_current {
            map.remove(key);
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
        let mut loader = Some(loader);

        loop {
            let (call, is_leader) = {
                let mut map = self.in_flight.lock();
                if let Some(existing) = map.get(&key) {
                    (Arc::clone(existing), false)
                } else {
                    let call = Arc::new(Call::new());
                    map.insert(key.clone(), Arc::clone(&call));
                    (call, true)
                }
            };

            if is_leader {
                // Install the guard before invoking user code so unwinding
                // always aborts and deregisters this exact call.
                let guard = LeaderGuard::new(self, &key, &call);
                let loader = loader
                    .take()
                    .expect("a follower loader cannot be consumed before retry");
                let value = loader();
                return guard.complete(value);
            }

            let mut state = call.state.lock();
            loop {
                match &*state {
                    CallState::Running => call.ready.wait(&mut state),
                    CallState::Ready(value) => return value.clone(),
                    CallState::Aborted => break,
                }
            }
            drop(state);
            // The failed leader never consumed this caller's loader. Retry
            // top-level coordination so one awakened follower replaces it.
        }
    }
}

struct LeaderGuard<'a, K, V>
where
    K: Eq + Hash,
{
    flight: &'a SingleFlight<K, V>,
    key: &'a K,
    call: &'a Arc<Call<V>>,
    armed: bool,
}

impl<'a, K, V> LeaderGuard<'a, K, V>
where
    K: Eq + Hash,
{
    fn new(flight: &'a SingleFlight<K, V>, key: &'a K, call: &'a Arc<Call<V>>) -> Self {
        Self {
            flight,
            key,
            call,
            armed: true,
        }
    }

    fn complete(mut self, value: V) -> V
    where
        V: Clone,
    {
        let shared = value.clone();
        let mut state = self.call.state.lock();
        debug_assert!(matches!(*state, CallState::Running));
        *state = CallState::Ready(shared);
        self.call.ready.notify_all();
        self.flight.remove_call(self.key, self.call);
        drop(state);
        self.armed = false;
        value
    }
}

impl<K, V> Drop for LeaderGuard<'_, K, V>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        let mut state = self.call.state.lock();
        debug_assert!(matches!(*state, CallState::Running));
        *state = CallState::Aborted;
        self.call.ready.notify_all();
        self.flight.remove_call(self.key, self.call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Condvar as StdCondvar, Mutex as StdMutex};
    use std::thread;
    use std::time::{Duration, Instant};

    const WATCHDOG: Duration = Duration::from_secs(5);

    struct Gate {
        open: StdMutex<bool>,
        ready: StdCondvar,
    }

    impl Gate {
        fn closed() -> Self {
            Self {
                open: StdMutex::new(false),
                ready: StdCondvar::new(),
            }
        }

        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.ready.wait(open).unwrap();
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.ready.notify_all();
        }
    }

    fn wait_for_references<V>(call: &Arc<Call<V>>, expected: usize) {
        let deadline = Instant::now() + WATCHDOG;
        while Arc::strong_count(call) < expected {
            assert!(
                Instant::now() < deadline,
                "call gained {} of {expected} expected references",
                Arc::strong_count(call)
            );
            thread::yield_now();
        }
    }

    #[test]
    fn panic_wakes_followers_and_coalesces_one_retry() {
        const FOLLOWERS: usize = 4;
        const KEY: usize = 7;

        let flight = Arc::new(SingleFlight::<usize, Arc<usize>>::new());
        let retry_operations = Arc::new(AtomicUsize::new(0));
        let retry_gate = Arc::new(Gate::closed());
        let (leader_started_tx, leader_started_rx) = mpsc::sync_channel(0);
        let (panic_tx, panic_rx) = mpsc::sync_channel(0);
        let (retry_started_tx, retry_started_rx) = mpsc::channel();

        let leader_flight = Arc::clone(&flight);
        let leader = thread::spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                leader_flight.run(KEY, || {
                    leader_started_tx.send(()).unwrap();
                    panic_rx.recv().unwrap();
                    panic!("leader failed");
                })
            }))
            .is_err()
        });

        leader_started_rx.recv_timeout(WATCHDOG).unwrap();
        let aborted_call = flight.in_flight.lock().get(&KEY).cloned().unwrap();
        let original_references = Arc::strong_count(&aborted_call);

        let followers = (0..FOLLOWERS)
            .map(|_| {
                let flight = Arc::clone(&flight);
                let retry_operations = Arc::clone(&retry_operations);
                let retry_gate = Arc::clone(&retry_gate);
                let retry_started_tx = retry_started_tx.clone();
                thread::spawn(move || {
                    flight.run(KEY, || {
                        retry_operations.fetch_add(1, Ordering::SeqCst);
                        retry_started_tx.send(()).unwrap();
                        retry_gate.wait();
                        Arc::new(42)
                    })
                })
            })
            .collect::<Vec<_>>();

        // Every follower has cloned the original call before it is allowed to
        // panic, so the wakeup assertion does not depend on scheduler timing.
        wait_for_references(&aborted_call, original_references + FOLLOWERS);
        panic_tx.send(()).unwrap();
        retry_started_rx.recv_timeout(WATCHDOG).unwrap();

        let retry_call = flight.in_flight.lock().get(&KEY).cloned().unwrap();
        assert!(!Arc::ptr_eq(&aborted_call, &retry_call));
        // Map + replacement leader + this test + the other followers.
        wait_for_references(&retry_call, FOLLOWERS + 2);
        retry_gate.open();

        assert!(leader.join().unwrap());
        let results = followers
            .into_iter()
            .map(|follower| follower.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(retry_operations.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|result| **result == 42));
        assert!(
            results[1..]
                .iter()
                .all(|result| Arc::ptr_eq(&results[0], result))
        );

        // Retaining the completed Call must not retain it in the key map.
        assert!(flight.in_flight.lock().is_empty());
        let fresh_operations = AtomicUsize::new(0);
        let fresh = flight.run(KEY, || {
            fresh_operations.fetch_add(1, Ordering::SeqCst);
            Arc::new(99)
        });
        assert_eq!(*fresh, 99);
        assert_eq!(fresh_operations.load(Ordering::SeqCst), 1);
    }

    struct CloneProbe {
        value: Arc<usize>,
        clones: Arc<AtomicUsize>,
    }

    impl Clone for CloneProbe {
        fn clone(&self) -> Self {
            self.clones.fetch_add(1, Ordering::SeqCst);
            Self {
                value: Arc::clone(&self.value),
                clones: Arc::clone(&self.clones),
            }
        }
    }

    #[test]
    fn successful_call_coalesces_with_one_clone_per_recipient() {
        const FOLLOWERS: usize = 4;
        const KEY: usize = 11;

        let flight = Arc::new(SingleFlight::<usize, CloneProbe>::new());
        let operations = Arc::new(AtomicUsize::new(0));
        let clones = Arc::new(AtomicUsize::new(0));
        let value = Arc::new(73);
        let (leader_started_tx, leader_started_rx) = mpsc::sync_channel(0);
        let (release_tx, release_rx) = mpsc::sync_channel(0);

        let leader_flight = Arc::clone(&flight);
        let leader_operations = Arc::clone(&operations);
        let leader_clones = Arc::clone(&clones);
        let leader_value = Arc::clone(&value);
        let leader = thread::spawn(move || {
            leader_flight.run(KEY, || {
                leader_operations.fetch_add(1, Ordering::SeqCst);
                leader_started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                CloneProbe {
                    value: leader_value,
                    clones: leader_clones,
                }
            })
        });

        leader_started_rx.recv_timeout(WATCHDOG).unwrap();
        let call = flight.in_flight.lock().get(&KEY).cloned().unwrap();
        let original_references = Arc::strong_count(&call);
        let followers = (0..FOLLOWERS)
            .map(|_| {
                let flight = Arc::clone(&flight);
                let operations = Arc::clone(&operations);
                let clones = Arc::clone(&clones);
                thread::spawn(move || {
                    flight.run(KEY, || {
                        operations.fetch_add(1, Ordering::SeqCst);
                        CloneProbe {
                            value: Arc::new(999),
                            clones,
                        }
                    })
                })
            })
            .collect::<Vec<_>>();

        wait_for_references(&call, original_references + FOLLOWERS);
        release_tx.send(()).unwrap();

        let leader_result = leader.join().unwrap();
        let follower_results = followers
            .into_iter()
            .map(|follower| follower.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(operations.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&leader_result.value, &value));
        assert!(
            follower_results
                .iter()
                .all(|result| Arc::ptr_eq(&result.value, &value))
        );
        // One clone is stored for the completed Call; each follower clones it.
        assert_eq!(clones.load(Ordering::SeqCst), FOLLOWERS + 1);
    }
}
