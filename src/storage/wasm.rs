use crate::counter::Counter;
use crate::limit::Limit;
use crate::storage::{Storage, StorageErr};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::iter::FromIterator;
use std::time::{Duration, SystemTime};

// This is a storage implementation that can be compiled to WASM. It is very
// similar to the "InMemory" one. The InMemory implementation cannot be used in
// WASM, because it relies on std:time functions. This implementation avoids
// that.

pub trait Clock: Sync + Send {
    fn get_current_time(&self) -> SystemTime;
}

pub struct CacheEntry<V> {
    pub value: V,
    pub expires_at: SystemTime,
}

impl<V: Copy> CacheEntry<V> {
    fn is_expired(&self, current_time: SystemTime) -> bool {
        current_time > self.expires_at
    }
}

pub struct Cache<K: Eq + Hash, V: Copy> {
    pub map: HashMap<K, CacheEntry<V>>,
}

impl<K: Eq + Hash + Clone, V: Copy> Cache<K, V> {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    pub fn get(&self, key: &K) -> Option<&CacheEntry<V>> {
        self.map.get(&key)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut CacheEntry<V>> {
        self.map.get_mut(&key)
    }

    pub fn insert(&mut self, key: &K, value: V, expires_at: SystemTime) {
        self.map
            .insert(key.clone(), CacheEntry { value, expires_at });
    }

    pub fn remove(&mut self, key: &K) {
        self.map.remove(key);
    }

    pub fn get_all(&mut self, current_time: SystemTime) -> Vec<(K, V, SystemTime)> {
        let iterator = self
            .map
            .iter()
            .filter(|(_key, cache_entry)| !cache_entry.is_expired(current_time))
            .map(|(key, cache_entry)| (key.clone(), cache_entry.value, cache_entry.expires_at));

        Vec::from_iter(iterator)
    }
}

impl<K: Eq + Hash + Clone, V: Copy> Default for Cache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WasmStorage {
    limits_for_namespace: HashMap<String, HashMap<Limit, HashSet<Counter>>>,
    pub counters: Cache<Counter, i64>,
    pub clock: Box<dyn Clock>,
}

impl Storage for WasmStorage {
    fn add_limit(&mut self, limit: Limit) -> Result<(), StorageErr> {
        let namespace = limit.namespace().to_string();

        match self.limits_for_namespace.get_mut(&namespace) {
            Some(limits) => {
                limits.insert(limit, HashSet::new());
            }
            None => {
                let mut limits = HashMap::new();
                limits.insert(limit, HashSet::new());
                self.limits_for_namespace.insert(namespace, limits);
            }
        }

        Ok(())
    }

    fn get_limits(&self, namespace: &str) -> Result<HashSet<Limit>, StorageErr> {
        let limits = match self.limits_for_namespace.get(namespace) {
            Some(limits) => HashSet::from_iter(limits.keys().cloned()),
            None => HashSet::new(),
        };

        Ok(limits)
    }

    fn delete_limit(&mut self, limit: &Limit) -> Result<(), StorageErr> {
        self.delete_counters_of_limit(limit);

        if let Some(counters_by_limit) = self.limits_for_namespace.get_mut(limit.namespace()) {
            counters_by_limit.remove(limit);
        }

        Ok(())
    }

    fn delete_limits(&mut self, namespace: &str) -> Result<(), StorageErr> {
        self.delete_counters_in_namespace(namespace);
        self.limits_for_namespace.remove(namespace);
        Ok(())
    }

    fn is_within_limits(&self, counter: &Counter, delta: i64) -> Result<bool, StorageErr> {
        let within_limits = match self.counters.get(counter) {
            Some(entry) => {
                if entry.is_expired(self.clock.get_current_time()) {
                    true
                } else {
                    entry.value - delta >= 0
                }
            }
            None => true,
        };

        Ok(within_limits)
    }

    fn update_counter(&mut self, counter: &Counter, delta: i64) -> Result<(), StorageErr> {
        match self.counters.get_mut(counter) {
            Some(entry) => {
                if entry.is_expired(self.clock.get_current_time()) {
                    // TODO: remove duplication. "None" branch is identical.
                    self.counters.insert(
                        counter,
                        counter.max_value() - delta,
                        self.clock.get_current_time() + Duration::from_secs(counter.seconds()),
                    );
                } else {
                    entry.value -= delta;
                }
            }
            None => {
                self.counters.insert(
                    counter,
                    counter.max_value() - delta,
                    self.clock.get_current_time() + Duration::from_secs(counter.seconds()),
                );

                self.add_counter_limit_association(counter);
            }
        };

        Ok(())
    }

    fn get_counters(
        &mut self,
        namespace: &str,
    ) -> Result<Vec<(Counter, i64, Duration)>, StorageErr> {
        // TODO: optimize to avoid iterating over all of them.

        Ok(self
            .counters
            .get_all(self.clock.get_current_time())
            .iter()
            .filter(|(counter, _, _)| counter.namespace() == namespace)
            .map(|(counter, value, expires_at)| {
                (
                    counter.clone(),
                    *value,
                    expires_at.duration_since(SystemTime::UNIX_EPOCH).unwrap()
                        - self
                            .clock
                            .get_current_time()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap(),
                )
            })
            .collect())
    }
}

impl WasmStorage {
    pub fn new(clock: Box<impl Clock + 'static>) -> Self {
        Self {
            limits_for_namespace: HashMap::new(),
            counters: Cache::default(),
            clock,
        }
    }

    pub fn add_counter(&mut self, counter: &Counter, value: i64, expires_at: SystemTime) {
        self.counters.insert(counter, value, expires_at);
    }

    fn delete_counters_in_namespace(&mut self, namespace: &str) {
        if let Some(counters_by_limit) = self.limits_for_namespace.get(namespace) {
            for counter in counters_by_limit.values().flatten() {
                self.counters.remove(counter);
            }
        }
    }

    fn delete_counters_of_limit(&mut self, limit: &Limit) {
        if let Some(counters_by_limit) = self.limits_for_namespace.get(limit.namespace()) {
            if let Some(counters) = counters_by_limit.get(limit) {
                for counter in counters.iter() {
                    self.counters.remove(counter);
                }
            }
        }
    }

    fn add_counter_limit_association(&mut self, counter: &Counter) {
        let namespace = counter.limit().namespace();

        if let Some(counters_by_limit) = self.limits_for_namespace.get_mut(namespace) {
            counters_by_limit
                .get_mut(counter.limit())
                .unwrap()
                .insert(counter.clone());
        }
    }
}