use std::collections::HashSet;
use std::hash::Hash;

use crate::prefix_lookup::PrefixLookup;

/// A product pattern descriptor used by `OutputIndex`.
pub trait ProductPattern {
    fn constant_prefix(&self) -> String;
    fn constant_suffix(&self) -> String;
}

/// Lookup index for rules queried by target file paths, matching Snakemake's `OutputIndex`.
#[derive(Debug, Clone)]
pub struct OutputIndex<R> {
    lookup: PrefixLookup<(R, String)>,
}

impl<R: Clone + Eq + Hash> OutputIndex<R> {
    pub fn new<P, F>(rules: Vec<R>, products_fn: F) -> Self
    where
        P: ProductPattern,
        F: Fn(&R) -> Vec<P>,
    {
        let mut entries = Vec::new();
        for rule in rules {
            for product in products_fn(&rule) {
                let prefix = product.constant_prefix();
                let suffix = product.constant_suffix();
                entries.push((prefix, (rule.clone(), suffix)));
            }
        }
        Self {
            lookup: PrefixLookup::new(entries),
        }
    }

    /// Returns all rules whose product prefix and suffix match `targetfile`.
    pub fn match_rules(&self, targetfile: &str) -> HashSet<R> {
        self.lookup
            .match_iter(targetfile)
            .into_iter()
            .filter_map(|(_prefix, (rule, suffix))| {
                if targetfile.ends_with(suffix.as_str()) {
                    Some(rule.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns all rules matching prefix/suffix and satisfying `is_producer(rule, targetfile)`.
    pub fn match_producers<F>(&self, targetfile: &str, is_producer: F) -> HashSet<R>
    where
        F: Fn(&R, &str) -> bool,
    {
        self.match_rules(targetfile)
            .into_iter()
            .filter(|r| is_producer(r, targetfile))
            .collect()
    }
}
