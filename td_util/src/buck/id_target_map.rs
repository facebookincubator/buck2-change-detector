/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashMap;

use td_util::no_hash::BuildNoHash;
use tracing::warn;

use crate::package_resolver::PackageResolver;
use crate::target_graph::PackageId;
use crate::target_graph::TargetId;
use crate::types::TargetPattern;

/// Maps target patterns to values, like [`crate::target_map::TargetMap`], but
/// keyed by the content-hash ids of [`crate::target_graph::TargetGraph`] so a
/// lookup for a target label never interns the label.
///
/// Labels and package paths are hashed on the fly with the same hash the graph
/// ids use, so a label that was never stored in the graph still resolves to
/// the id it would have.
pub struct IdTargetMap<T> {
    literal: HashMap<TargetId, Vec<T>, BuildNoHash>,
    non_recursive_pattern: HashMap<PackageId, Vec<T>, BuildNoHash>,
    recursive_pattern: PackageResolver<Vec<T>>,
}

impl<T> Default for IdTargetMap<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// `cell//pkg` for `cell//pkg:name`. A label without `:` is returned as is.
fn package_of_label(label: &str) -> &str {
    label.rsplit_once(':').map_or(label, |(package, _)| package)
}

impl<T> IdTargetMap<T> {
    pub fn new() -> Self {
        Self {
            literal: HashMap::with_hasher(BuildNoHash::default()),
            non_recursive_pattern: HashMap::with_hasher(BuildNoHash::default()),
            recursive_pattern: PackageResolver::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.literal.is_empty()
            && self.non_recursive_pattern.is_empty()
            && self.recursive_pattern.is_empty()
    }

    /// Register `value` for the exact target `label` (`cell//pkg:name`).
    pub fn insert_label(&mut self, label: &str, value: T) {
        let target_id: TargetId = label.parse().expect("TargetId parsing is infallible");
        self.literal.entry(target_id).or_default().push(value);
    }

    /// Register `value` for every target matched by `pattern`.
    pub fn insert_pattern(&mut self, pattern: &TargetPattern, value: T) {
        if pattern.is_specific_target() {
            self.insert_label(pattern.as_str(), value);
        } else if let Some(package) = pattern.as_package_pattern() {
            let package_id: PackageId = package
                .as_str()
                .parse()
                .expect("PackageId parsing is infallible");
            self.non_recursive_pattern
                .entry(package_id)
                .or_default()
                .push(value);
        } else if let Some(package) = pattern.as_recursive_pattern() {
            self.recursive_pattern.update(&package, move |old| {
                let mut res = old.unwrap_or_default();
                res.push(value);
                res
            });
        } else {
            warn!("Ignored invalid target pattern, `{}`", pattern)
        }
    }

    /// Visit every value registered for the target `label`: exact matches,
    /// then its package pattern, then enclosing recursive patterns top-down.
    pub fn for_each_matching<'a>(&'a self, label: &str, mut f: impl FnMut(&'a T)) {
        let target_id: TargetId = label.parse().expect("TargetId parsing is infallible");
        if let Some(values) = self.literal.get(&target_id) {
            values.iter().for_each(&mut f);
        }
        if self.non_recursive_pattern.is_empty() && self.recursive_pattern.is_empty() {
            return;
        }
        let package = package_of_label(label);
        let package_id: PackageId = package.parse().expect("PackageId parsing is infallible");
        if let Some(values) = self.non_recursive_pattern.get(&package_id) {
            values.iter().for_each(&mut f);
        }
        self.recursive_pattern
            .for_each_at_or_above(package, |values| values.iter().for_each(&mut f));
    }

    /// Whether any value is registered for the target `label`.
    pub fn has_matching(&self, label: &str) -> bool {
        let target_id: TargetId = label.parse().expect("TargetId parsing is infallible");
        if self.literal.contains_key(&target_id) {
            return true;
        }
        if self.non_recursive_pattern.is_empty() && self.recursive_pattern.is_empty() {
            return false;
        }
        let package = package_of_label(label);
        let package_id: PackageId = package.parse().expect("PackageId parsing is infallible");
        self.non_recursive_pattern.contains_key(&package_id)
            || self.recursive_pattern.has_at_or_above(package)
    }

    /// All values registered for the target `label`, in `for_each_matching` order.
    pub fn get(&self, label: &str) -> Vec<&T> {
        let mut res = Vec::new();
        self.for_each_matching(label, |value| res.push(value));
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target_map::TargetMap;
    use crate::types::TargetLabel;

    /// One pattern of every shape the two maps can hold.
    const PATTERNS: &[&str] = &[
        "foo//bar:baz",
        "foo//bar:",
        "foo//bar/...",
        "foo//...",
        "foo//:",
        "bar//bar:baz",
    ];

    /// Labels probing the boundaries between those shapes: the package itself,
    /// a sibling in it, a subpackage, a package that only shares a string
    /// prefix, the cell root, an unrelated package, another cell.
    const LABELS: &[&str] = &[
        "foo//bar:baz",
        "foo//bar:quz",
        "foo//bar/sub:x",
        "foo//bard:x",
        "foo//:baz",
        "foo//moo:boo",
        "bar//bar:baz",
        "none//moo:boo",
    ];

    /// `IdTargetMap` must answer exactly what `TargetMap` answers, in the same
    /// order, for every pattern shape.
    #[test]
    fn test_id_target_map_matches_target_map() {
        let mut interning: TargetMap<usize> = TargetMap::new();
        let mut by_id: IdTargetMap<usize> = IdTargetMap::new();
        for (value, pattern) in PATTERNS.iter().enumerate() {
            interning.insert_pattern(&TargetPattern::new(pattern), value);
            by_id.insert_pattern(&TargetPattern::new(pattern), value);
        }

        for label in LABELS {
            let expected: Vec<usize> = interning
                .get(&TargetLabel::new(label))
                .copied()
                .collect::<Vec<_>>();
            let actual: Vec<usize> = by_id.get(label).into_iter().copied().collect();
            assert_eq!(actual, expected, "values matching `{label}`");
            assert_eq!(
                by_id.has_matching(label),
                !expected.is_empty(),
                "has_matching(`{label}`)"
            );
        }
    }

    #[test]
    fn test_insert_label_matches_insert_of_the_same_pattern() {
        let mut by_id: IdTargetMap<i32> = IdTargetMap::new();
        by_id.insert_label("foo//bar:baz", 1);
        by_id.insert_pattern(&TargetPattern::new("foo//bar:baz"), 2);
        by_id.insert_label("foo//:root", 3);
        assert_eq!(
            by_id
                .get("foo//bar:baz")
                .into_iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            by_id
                .get("foo//:root")
                .into_iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn test_is_empty() {
        assert!(IdTargetMap::<i32>::new().is_empty());
        for pattern in PATTERNS {
            let mut by_id: IdTargetMap<i32> = IdTargetMap::new();
            by_id.insert_pattern(&TargetPattern::new(pattern), 1);
            assert!(!by_id.is_empty(), "not empty after inserting `{pattern}`");
        }
    }

    #[test]
    fn test_package_of_label() {
        assert_eq!(package_of_label("foo//bar:baz"), "foo//bar");
        assert_eq!(package_of_label("foo//:baz"), "foo//");
        assert_eq!(package_of_label("foo//bar"), "foo//bar");
    }
}
