/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeSet;

use crate::cells::CellInfo;
use crate::types::CellPath;

/// Set of restricted (tented) path prefixes fetched from Source Control Service.
///
/// Used to determine whether a Buck target lives under a tented path.
#[derive(Default)]
pub struct RestrictedPaths {
    /// Repo-relative restricted path prefixes from SCS, each paired with ACL names
    /// (e.g. `("fbcode/genai/secret", vec!["acl1", "acl2"])`).
    paths: Vec<(String, Vec<String>)>,
}

impl RestrictedPaths {
    /// Creates a `RestrictedPaths` from a list of `(path_prefix, acl_names)` tuples.
    pub fn new(paths: Vec<(String, Vec<String>)>) -> Self {
        Self { paths }
    }

    /// Returns the set of ACLs from all matching restricted path prefixes.
    ///
    /// An empty set means the path is not tented. A non-empty set contains the
    /// combined ACLs from all matching prefixes (supporting nested tents).
    pub fn is_tented(&self, path: &CellPath, cells: &CellInfo) -> BTreeSet<String> {
        let repo_relative = match cells.resolve(path) {
            Ok(path) => path,
            Err(_) => return BTreeSet::new(),
        };
        let repo_relative = repo_relative.as_str();

        self.paths
            .iter()
            .filter_map(|(prefix, prefix_acls)| {
                repo_relative
                    .strip_prefix(prefix.as_str())
                    .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                    .map(|_| prefix_acls)
            })
            .flat_map(|acls| acls.iter().cloned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn restricted_from_paths(paths: Vec<&str>) -> RestrictedPaths {
        RestrictedPaths::new(
            paths
                .into_iter()
                .map(|s| (s.to_owned(), vec!["default_acl".to_owned()]))
                .collect(),
        )
    }

    #[rstest]
    #[case::empty_restricted_paths(vec![], "fbcode//bar", false)]
    #[case::exact_match(vec!["fbcode/bar"], "fbcode//bar", true)]
    #[case::prefix_match(vec!["fbcode/genai"], "fbcode//genai/secret/deep", true)]
    #[case::no_match(vec!["fbcode/genai"], "fbcode//other/path", false)]
    #[case::no_partial_segment_match(vec!["fbcode/gen"], "fbcode//genai/secret", false)]
    #[case::suffix_segment_no_match(vec!["foo/bar"], "fbcode//foo/bar", false)]
    fn test_is_tented(#[case] restricted: Vec<&str>, #[case] path: &str, #[case] expected: bool) {
        let cells = CellInfo::testing();
        let restricted = restricted_from_paths(restricted);
        let cell_path = CellPath::new(path);
        let result = restricted.is_tented(&cell_path, &cells);
        assert_eq!(!result.is_empty(), expected);
    }

    #[rstest]
    #[case::returns_matching_acls(
        vec![("fbcode/genai/secret", vec!["acl_read", "acl_write"])],
        "fbcode//genai/secret/deep",
        BTreeSet::from(["acl_read".to_owned(), "acl_write".to_owned()]),
    )]
    #[case::non_matching_path_returns_empty(
        vec![("fbcode/genai/secret", vec!["acl_read", "acl_write"])],
        "fbcode//other/path",
        BTreeSet::new(),
    )]
    #[case::nested_tents_combine_acls(
        vec![("fbcode/genai", vec!["outer_acl"]), ("fbcode/genai/secret", vec!["inner_acl"])],
        "fbcode//genai/secret/deep",
        BTreeSet::from(["outer_acl".to_owned(), "inner_acl".to_owned()]),
    )]
    #[case::nested_outer_only(
        vec![("fbcode/genai", vec!["outer_acl"]), ("fbcode/genai/secret", vec!["inner_acl"])],
        "fbcode//genai/other",
        BTreeSet::from(["outer_acl".to_owned()]),
    )]
    fn test_is_tented_acls(
        #[case] restricted: Vec<(&str, Vec<&str>)>,
        #[case] path: &str,
        #[case] expected: BTreeSet<String>,
    ) {
        let cells = CellInfo::testing();
        let restricted = RestrictedPaths::new(
            restricted
                .into_iter()
                .map(|(p, acls)| {
                    (
                        p.to_owned(),
                        acls.into_iter().map(|a| a.to_owned()).collect(),
                    )
                })
                .collect(),
        );
        let cell_path = CellPath::new(path);
        assert_eq!(restricted.is_tented(&cell_path, &cells), expected);
    }

    #[test]
    fn test_unknown_cell_not_tented() {
        let cells = CellInfo::testing();
        let restricted = restricted_from_paths(vec!["unknown/path"]);

        // CellInfo::testing() doesn't know about "unknown" cell, so resolve fails
        let cell_path = CellPath::new("unknown//path/sub");
        assert!(restricted.is_tented(&cell_path, &cells).is_empty());
    }
}
