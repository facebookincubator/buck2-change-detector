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

use crate::types::Package;

/// Perform the `PACKAGE` file resolution algorithm.
#[derive(Debug)]
pub struct PackageResolver<T> {
    value: Option<T>,
    children: HashMap<String, PackageResolver<T>>,
}

impl<T> Default for PackageResolver<T> {
    fn default() -> Self {
        Self {
            value: None,
            children: HashMap::new(),
        }
    }
}

impl<T> PackageResolver<T> {
    /// Create a new `PackageResolver`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the type empty, only true if `insert` has never be called.
    pub fn is_empty(&self) -> bool {
        self.value.is_none() && self.children.is_empty()
    }

    /// Split a `Package` up into a series of strings that are used for indexing the maps.
    fn package_parts(package: &Package) -> impl Iterator<Item = &str> {
        Self::package_path_parts(package.as_str())
    }

    /// `package_parts` for a package path that has not been interned.
    fn package_path_parts(mut s: &str) -> impl Iterator<Item = &str> {
        // Note that a string such as `foo//bar` will currently turn into `["foo", "", "bar"]`.
        // We could remove the empty string part, but its simpler not to.
        if s.ends_with("//") {
            // corner case if we have a PACKAGE at the root of a cell
            s = &s[0..s.len() - 1];
        }
        s.split('/')
    }

    /// Insert a value found at `Package` location.
    pub fn insert(&mut self, package: &Package, value: T) {
        self.update(package, |_| value)
    }

    /// Update a value found at `Package` location. `None` if there is not already a value there.
    pub fn update(&mut self, package: &Package, update: impl FnOnce(Option<T>) -> T) {
        let mut mp = self;
        for x in Self::package_parts(package) {
            mp = mp.children.entry(x.to_owned()).or_default();
        }
        mp.value = Some(update(mp.value.take()));
    }

    /// Get all the values that a `Package` would encounter, namely all those that were inserted at or above it.
    /// The result will be the values starting at the top and going downwards.
    /// In Buck2 these would then be processed in _reverse_ order.
    pub fn get(&self, package: &Package) -> Vec<&T> {
        let mut res = Vec::new();
        self.for_each_at_or_above(package.as_str(), |value| res.push(value));
        res
    }

    /// Whether a value exists at this package or an ancestor, stopping at the first match.
    pub fn has_at_or_above(&self, package_path: &str) -> bool {
        let mut node = self;
        if node.value.is_some() {
            return true;
        }
        for part in Self::package_path_parts(package_path) {
            let Some(child) = node.children.get(part) else {
                return false;
            };
            if child.value.is_some() {
                return true;
            }
            node = child;
        }
        false
    }

    /// Like `get`, but takes the package path as a plain string (no interning)
    /// and visits the values top-down instead of collecting them.
    pub fn for_each_at_or_above<'a>(&'a self, package_path: &str, mut f: impl FnMut(&'a T)) {
        let mut mp = self;
        if let Some(value) = mp.value.as_ref() {
            f(value);
        }
        for x in Self::package_path_parts(package_path) {
            match mp.children.get(x) {
                None => break,
                Some(mp2) => {
                    if let Some(value) = mp2.value.as_ref() {
                        f(value);
                    }
                    mp = mp2;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_package_resolver_is_empty() {
        let mut p = PackageResolver::new();
        assert!(p.is_empty());
        p.insert(&Package::new("foo//bar"), 1);
        assert!(!p.is_empty());
    }

    #[test]
    fn test_package_resolver_insert() {
        let mut p = PackageResolver::new();
        p.insert(&Package::new("foo//"), 1);
        p.insert(&Package::new("foo//bar/baz"), 2);
        assert_eq!(p.get(&Package::new("foo//")), vec![&1]);
        assert_eq!(p.get(&Package::new("foo//bar")), vec![&1]);
        assert_eq!(p.get(&Package::new("foo//bar/baz")), vec![&1, &2]);
        assert_eq!(p.get(&Package::new("foo//bar/baz/qux")), vec![&1, &2]);
        assert_eq!(p.get(&Package::new("other//bar")), Vec::<&i32>::new());
    }

    #[test]
    fn test_has_at_or_above_respects_package_boundaries() {
        let mut p = PackageResolver::new();
        assert!(!p.has_at_or_above("foo//bar"));
        p.insert(&Package::new("foo//bar"), 1);
        assert!(p.has_at_or_above("foo//bar"));
        assert!(p.has_at_or_above("foo//bar/sub"));
        assert!(!p.has_at_or_above("foo//bard"));
        assert!(!p.has_at_or_above("foo//"));
        assert!(!p.has_at_or_above("other//bar"));
        p.insert(&Package::new("foo//"), 2);
        assert!(p.has_at_or_above("foo//"));
        assert!(p.has_at_or_above("foo//other"));
    }
}
