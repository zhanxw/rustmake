use std::collections::{BTreeMap, HashSet};
use std::hash::Hash;

/// Efficient prefix lookup data structure matching Snakemake's `PrefixLookup`.
#[derive(Debug, Clone)]
pub struct PrefixLookup<V> {
    entries: Vec<(String, Vec<V>)>,
}

impl<V> PrefixLookup<V> {
    pub fn new<I>(entries: I) -> Self
    where
        I: IntoIterator<Item = (String, V)>,
    {
        let mut grouped: BTreeMap<String, Vec<V>> = BTreeMap::new();
        for (key, value) in entries {
            grouped.entry(key).or_default().push(value);
        }
        Self {
            entries: grouped.into_iter().collect(),
        }
    }

    /// Iterate over all `(prefix_key, value)` pairs where `prefix_key` is a prefix of `query`.
    pub fn match_iter(&self, query: &str) -> Vec<(&str, &V)> {
        let mut out = Vec::new();
        let mut stop_idx = self.entries.len();
        let mut current = query.to_string();

        while stop_idx > 0 {
            // bisect_right over self.entries[..stop_idx] comparing key <= current
            let pos = self.entries[..stop_idx].partition_point(|(k, _)| k.as_str() <= current.as_str());
            stop_idx = pos;
            if stop_idx == 0 {
                break;
            }

            let (k, vals) = &self.entries[stop_idx - 1];
            if current.starts_with(k.as_str()) {
                for v in vals {
                    out.push((k.as_str(), v));
                }
            }

            if current.is_empty() || k.is_empty() {
                break;
            }

            current = common_prefix_stepdown(k, &current);
        }
        out
    }
}

impl<V: Eq + Hash + Clone> PrefixLookup<V> {
    /// Returns a set of all values whose attached key is a prefix of `query`.
    pub fn match_set(&self, query: &str) -> HashSet<V> {
        self.match_iter(query)
            .into_iter()
            .map(|(_, v)| v.clone())
            .collect()
    }
}

/// Gives the common prefix of `s1` and `s2`, except if `s1` is a prefix of `s2` (or identical),
/// returns `s1` minus one Unicode scalar value (matching Python's `PrefixLookup._common_prefix`).
fn common_prefix_stepdown(s1: &str, s2: &str) -> String {
    let mut byte_end = 0usize;
    let mut last_char_start = 0usize;
    let mut broke = false;

    for ((idx1, c1), c2) in s1.char_indices().zip(s2.chars()) {
        if c1 != c2 {
            byte_end = idx1;
            broke = true;
            break;
        }
        last_char_start = idx1;
        byte_end = idx1 + c1.len_utf8();
    }

    if broke {
        s1[..byte_end].to_string()
    } else {
        // Reached the end of the shorter string without mismatch.
        // Python's loop keeps `i` at the index of the final zipped char, so `s1[:i]` drops that last char.
        s1[..last_char_start].to_string()
    }
}
