//! A join-side holding map with hard bounds: at most `max_count` entries and `max_bytes`
//! accounted bytes, oldest entries evicted first (counted). The comparator's pending FL records
//! and agave frames and the shadow check's full results live in these, so the comparator alone
//! can never approach the fast lane's global memory cap.

use {
    crate::mem::Gauge,
    std::{
        collections::{HashMap, VecDeque},
        hash::Hash,
    },
};

pub struct Held<K, V> {
    map: HashMap<K, (V, i64, u64)>,
    order: VecDeque<(K, u64)>,
    next: u64,
    bytes: i64,
    max_count: usize,
    max_bytes: i64,
    gauge: &'static Gauge,
    /// Entries evicted to stay within the bounds (cumulative).
    pub evicted: u64,
}

impl<K: Hash + Eq + Copy, V> Held<K, V> {
    pub fn new(max_count: usize, max_bytes: i64, gauge: &'static Gauge) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            next: 0,
            bytes: 0,
            max_count: max_count.max(1),
            max_bytes: max_bytes.max(1),
            gauge,
            evicted: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn bytes(&self) -> i64 {
        self.bytes
    }

    /// Insert (replacing an entry with the same key), then evict the oldest entries while over
    /// a bound. Returns the evicted values (the replaced one first).
    pub fn insert(&mut self, key: K, value: V, bytes: i64) -> Vec<V> {
        let mut out = Vec::new();
        if let Some(old) = self.remove(&key) {
            out.push(old);
        }
        let seq = self.next;
        self.next += 1;
        self.map.insert(key, (value, bytes, seq));
        self.order.push_back((key, seq));
        self.bytes += bytes;
        self.gauge.add(bytes);
        while self.map.len() > self.max_count || self.bytes > self.max_bytes {
            let Some((key, seq)) = self.order.pop_front() else {
                break;
            };
            if self.map.get(&key).is_some_and(|(_, _, s)| *s == seq) {
                if let Some(v) = self.remove(&key) {
                    self.evicted += 1;
                    out.push(v);
                }
            }
        }
        // Keep the order queue proportional to the map (removed keys leave stale entries).
        if self.order.len() > 2 * self.map.len() + 1024 {
            let map = &self.map;
            self.order
                .retain(|(k, s)| map.get(k).is_some_and(|(_, _, seq)| seq == s));
        }
        out
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let (value, bytes, _) = self.map.remove(key)?;
        self.bytes -= bytes;
        self.gauge.sub(bytes);
        Some(value)
    }

    /// Remove and return every entry for which `stale` holds.
    pub fn remove_where(&mut self, mut stale: impl FnMut(&K, &V) -> bool) -> Vec<(K, V)> {
        let keys: Vec<K> = self
            .map
            .iter()
            .filter(|(k, (v, _, _))| stale(k, v))
            .map(|(k, _)| *k)
            .collect();
        keys.into_iter()
            .filter_map(|k| self.remove(&k).map(|v| (k, v)))
            .collect()
    }

    pub fn clear(&mut self) {
        self.gauge.sub(self.bytes);
        self.bytes = 0;
        self.map = HashMap::new();
        self.order = VecDeque::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static G: Gauge = Gauge::new();

    #[test]
    fn test_bounds_and_accounting() {
        let mut h: Held<u32, u32> = Held::new(3, 1_000, &G);
        assert!(h.insert(1, 10, 100).is_empty());
        assert!(h.insert(2, 20, 100).is_empty());
        assert!(h.insert(3, 30, 100).is_empty());
        assert_eq!(h.insert(4, 40, 100), vec![10], "count bound evicts the oldest");
        assert_eq!(h.evicted, 1);
        assert_eq!(h.remove(&2), Some(20));
        assert_eq!(h.insert(5, 50, 901), vec![30, 40], "byte bound evicts the oldest");
        assert_eq!(h.len(), 1);
        assert_eq!(h.bytes(), 901);
        assert_eq!(G.get(), 901);
        assert_eq!(h.insert(5, 51, 10), vec![50], "replace");
        assert_eq!(h.evicted, 3);
        for i in 0..10_000 {
            h.insert(100 + i, i, 1);
            h.remove(&(100 + i));
        }
        assert!(h.order.len() < 2_000, "order queue compacted");
        h.clear();
        assert_eq!(G.get(), 0);
    }
}
