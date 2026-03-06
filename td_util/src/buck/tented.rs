/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use crate::cells::CellInfo;
use crate::types::CellPath;

/// Set of restricted (tented) path prefixes fetched from Source Control Service.
///
/// Used to determine whether a Buck target lives under a tented path.
#[derive(Default)]
pub struct RestrictedPaths {
    /// Repo-relative restricted path prefixes from SCS (e.g. `"fbcode/genai/secret"`).
    paths: Vec<String>,
}

impl RestrictedPaths {
    /// Creates a `RestrictedPaths` from a list of repo-relative path prefixes.
    pub fn new(paths: Vec<String>) -> Self {
        Self { paths }
    }

    /// Returns `true` if the given path falls under a restricted path prefix.
    pub fn is_tented(&self, path: &CellPath, cells: &CellInfo) -> bool {
        let repo_relative = match cells.resolve(path) {
            Ok(path) => path,
            Err(_) => return false,
        };
        let repo_relative = repo_relative.as_str();

        self.paths.iter().any(|p| {
            repo_relative
                .strip_prefix(p.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn restricted_from_paths(paths: Vec<&str>) -> RestrictedPaths {
        RestrictedPaths::new(paths.into_iter().map(|s| s.to_owned()).collect())
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
        assert_eq!(restricted.is_tented(&cell_path, &cells), expected);
    }

    #[test]
    fn test_unknown_cell_not_tented() {
        let cells = CellInfo::testing();
        let restricted = restricted_from_paths(vec!["unknown/path"]);

        // CellInfo::testing() doesn't know about "unknown" cell, so resolve fails
        let cell_path = CellPath::new("unknown//path/sub");
        assert!(!restricted.is_tented(&cell_path, &cells));
    }
}
