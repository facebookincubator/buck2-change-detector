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

use tracing::warn;

use crate::cells::CellInfo;
use crate::types::CellPath;

const TENT_TAG_PREFIX: &str = "tent_";

/// Converts a `REPO_REGION` ACL name into a sandcastle tag.
///
/// For `REPO_REGION:repos/hg/<repo>/=<name>` ACLs, produces `tent_<name>`.
/// Returns `None` for non-`REPO_REGION` ACLs or malformed entries.
fn acl_to_tag(acl: &str) -> Option<String> {
    let ("REPO_REGION", data) = acl.split_once(':')? else {
        warn!(acl, "skipping non-REPO_REGION ACL");
        return None;
    };
    match data.rsplit_once('=') {
        Some((_, name)) if !name.is_empty() => Some(format!("{TENT_TAG_PREFIX}{name}")),
        _ => {
            warn!(acl, "malformed REPO_REGION ACL, expected '=<name>' suffix");
            None
        }
    }
}

/// A restricted path prefix and the ACL names protecting it.
#[derive(Clone, Debug)]
pub struct RestrictedPath {
    /// Repo-relative restricted path prefix (e.g. `"fbcode/genai/secret"`).
    pub path: String,
    /// ACL names protecting this path (e.g. `["REPO_REGION:repos/hg/fbsource/=titan"]`).
    pub acls: Vec<String>,
}

/// Set of restricted (tented) path prefixes fetched from Source Control Service.
///
/// Used to determine whether a Buck target lives under a tented path.
#[derive(Default)]
pub struct RestrictedPaths {
    entries: Vec<RestrictedPath>,
}

impl RestrictedPaths {
    /// Creates a `RestrictedPaths` from a list of `(path_prefix, acl_names)` tuples.
    ///
    /// Entries are sorted longest-path-first so the most specific prefix is
    /// matched before broader ones.
    pub fn new(paths: Vec<(String, Vec<String>)>) -> Self {
        let mut entries: Vec<_> = paths
            .into_iter()
            .map(|(path, acls)| RestrictedPath { path, acls })
            .collect();
        entries.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Self { entries }
    }

    /// Creates a `RestrictedPaths` from a list of path+ACL entries.
    ///
    /// Entries are sorted longest-path-first so the most specific prefix is
    /// matched before broader ones.
    pub fn new_with_acls(mut entries: Vec<RestrictedPath>) -> Self {
        entries.sort_by(|a, b| b.path.len().cmp(&a.path.len()));
        Self { entries }
    }

    /// Returns the sandcastle tags for the tented path this target falls under.
    ///
    /// Tags are derived from the ACL names protecting the restricted path.
    /// For `REPO_REGION:repos/hg/<repo>/=<name>` ACLs, produces `tent_<name>`.
    /// Non-`REPO_REGION` ACLs and malformed entries are skipped.
    /// Returns an empty set if the target is not under a restricted path or if
    /// no valid tags could be derived from the ACLs.
    pub fn tenting_tags(&self, path: &CellPath, cells: &CellInfo) -> BTreeSet<String> {
        let repo_relative = match cells.resolve(path) {
            Ok(path) => path,
            Err(_) => return BTreeSet::new(),
        };
        let repo_relative = repo_relative.as_str();

        self.entries
            .iter()
            .filter(|entry| {
                repo_relative
                    .strip_prefix(entry.path.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
            })
            .flat_map(|entry| entry.acls.iter().filter_map(|acl| acl_to_tag(acl)))
            .collect()
    }

    /// Returns `true` if the given path falls under a restricted path prefix
    /// and has valid tenting tags.
    pub fn is_tented(&self, path: &CellPath, cells: &CellInfo) -> bool {
        !self.tenting_tags(path, cells).is_empty()
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

    fn restricted_from_paths_with_acls(entries: Vec<(&str, Vec<&str>)>) -> RestrictedPaths {
        RestrictedPaths::new_with_acls(
            entries
                .into_iter()
                .map(|(path, acls)| RestrictedPath {
                    path: path.to_owned(),
                    acls: acls.into_iter().map(|s| s.to_owned()).collect(),
                })
                .collect(),
        )
    }

    #[rstest]
    #[case::empty_restricted_paths(vec![], "fbcode//bar", false)]
    #[case::exact_match(
        vec![("fbcode/bar", vec!["REPO_REGION:repos/hg/fbsource/=bar"])],
        "fbcode//bar",
        true
    )]
    #[case::prefix_match(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=genai"])],
        "fbcode//genai/secret/deep",
        true
    )]
    #[case::no_match(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=genai"])],
        "fbcode//other/path",
        false
    )]
    #[case::no_partial_segment_match(
        vec![("fbcode/gen", vec!["REPO_REGION:repos/hg/fbsource/=gen"])],
        "fbcode//genai/secret",
        false
    )]
    #[case::no_acls_not_tented(
        vec![("fbcode/bar", vec![])],
        "fbcode//bar",
        false
    )]
    fn test_is_tented(
        #[case] entries: Vec<(&str, Vec<&str>)>,
        #[case] path: &str,
        #[case] expected: bool,
    ) {
        let cells = CellInfo::testing();
        let restricted = if entries.is_empty() {
            restricted_from_paths(vec![])
        } else {
            restricted_from_paths_with_acls(entries)
        };
        let cell_path = CellPath::new(path);
        let result = restricted.is_tented(&cell_path, &cells);
        assert_eq!(result, expected);
    }

    #[rstest]
    #[case::returns_matching_tags(
        vec![("fbcode/genai/secret", vec!["REPO_REGION:repos/hg/fbsource/=read", "REPO_REGION:repos/hg/fbsource/=write"])],
        "fbcode//genai/secret/deep",
        BTreeSet::from(["tent_read".to_owned(), "tent_write".to_owned()]),
    )]
    #[case::non_matching_path_returns_empty(
        vec![("fbcode/genai/secret", vec!["REPO_REGION:repos/hg/fbsource/=read", "REPO_REGION:repos/hg/fbsource/=write"])],
        "fbcode//other/path",
        BTreeSet::new(),
    )]
    #[case::nested_tents_collects_all_tags(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=outer"]), ("fbcode/genai/secret", vec!["REPO_REGION:repos/hg/fbsource/=inner"])],
        "fbcode//genai/secret/deep",
        BTreeSet::from(["tent_outer".to_owned(), "tent_inner".to_owned()]),
    )]
    #[case::nested_outer_only(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=outer"]), ("fbcode/genai/secret", vec!["REPO_REGION:repos/hg/fbsource/=inner"])],
        "fbcode//genai/other",
        BTreeSet::from(["tent_outer".to_owned()]),
    )]
    fn test_tenting_tags_from_acls(
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
        assert_eq!(restricted.tenting_tags(&cell_path, &cells), expected);
    }

    #[rstest]
    #[case::no_acls(vec![("fbcode/bar", vec![])], "fbcode//bar", vec![])]
    #[case::with_single_repo_region_acl(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=genai"])],
        "fbcode//genai/secret",
        vec!["tent_genai"]
    )]
    #[case::with_multiple_repo_region_acls(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=genai", "REPO_REGION:repos/hg/fbsource/=titan"])],
        "fbcode//genai/secret",
        vec!["tent_genai", "tent_titan"]
    )]
    #[case::non_repo_region_acl_skipped(
        vec![("fbcode/genai", vec!["TIER:my-acl"])],
        "fbcode//genai/secret",
        vec![]
    )]
    #[case::mixed_acls_only_repo_region(
        vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=genai", "TIER:my-acl"])],
        "fbcode//genai/secret",
        vec!["tent_genai"]
    )]
    #[case::no_match(vec![("fbcode/genai", vec!["REPO_REGION:repos/hg/fbsource/=genai"])], "fbcode//other", vec![])]
    #[case::path_differs_from_region_name(
        vec![("fbcode/secret_project", vec!["REPO_REGION:repos/hg/fbsource/=titan"])],
        "fbcode//secret_project/src",
        vec!["tent_titan"]
    )]
    fn test_tenting_tags(
        #[case] entries: Vec<(&str, Vec<&str>)>,
        #[case] path: &str,
        #[case] expected: Vec<&str>,
    ) {
        let cells = CellInfo::testing();
        let restricted = restricted_from_paths_with_acls(entries);
        let cell_path = CellPath::new(path);
        assert_eq!(
            restricted.tenting_tags(&cell_path, &cells),
            expected
                .into_iter()
                .map(|s| s.to_owned())
                .collect::<BTreeSet<_>>()
        );
    }

    #[rstest]
    #[case::standard_repo_region("REPO_REGION:repos/hg/fbsource/=genai", Some("tent_genai"))]
    #[case::different_region("REPO_REGION:repos/hg/fbsource/=titan", Some("tent_titan"))]
    #[case::different_repo("REPO_REGION:repos/hg/configerator/=secrets", Some("tent_secrets"))]
    #[case::tier_acl_skipped("TIER:my-acl", None)]
    #[case::no_colon_skipped("malformed", None)]
    #[case::empty_name_after_equals("REPO_REGION:repos/hg/fbsource/=", None)]
    #[case::no_equals_sign("REPO_REGION:some-other-format", None)]
    fn test_acl_to_tag(#[case] acl: &str, #[case] expected: Option<&str>) {
        assert_eq!(acl_to_tag(acl), expected.map(|s| s.to_owned()));
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
