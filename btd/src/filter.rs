/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashSet;

use td_util_buck::targets::BuckTarget;
use td_util_buck::types::TargetLabel;

use crate::diff::ImpactTraceData;

pub fn filter_targets_with_superset<'a>(
    superset_content: String,
    recursive: Vec<Vec<(&'a BuckTarget, ImpactTraceData)>>,
) -> Vec<Vec<(&'a BuckTarget, ImpactTraceData)>> {
    // Parse superset content into a HashSet for fast lookup
    let superset: HashSet<TargetLabel> = superset_content
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| TargetLabel::new(line.trim()))
        .collect();

    // Filter recursive targets
    let filtered_recursive: Vec<Vec<(&BuckTarget, ImpactTraceData)>> = recursive
        .into_iter()
        .map(|level| {
            level
                .into_iter()
                .filter(|(target, _)| superset.contains(&target.label()))
                .collect()
        })
        .collect();

    filtered_recursive
}

#[cfg(test)]
mod tests {
    use td_util_buck::targets::BuckTarget;

    use super::*;
    use crate::diff::ImpactTraceData;

    fn create_test_target(name: &str, package: &str) -> BuckTarget {
        BuckTarget::testing(name, package, "prelude//rules.bzl:cxx_library")
    }

    #[test]
    fn test_filter_targets_with_superset_empty_superset() {
        let target1 = create_test_target("foo", "cell//pkg");
        let target2 = create_test_target("bar", "cell//pkg");
        let recursive = vec![
            vec![(&target1, ImpactTraceData::testing())],
            vec![(&target2, ImpactTraceData::testing())],
        ];

        let result = filter_targets_with_superset("".to_string(), recursive);

        // Empty superset should filter out all targets
        assert_eq!(result.len(), 2);
        assert!(result[0].is_empty());
        assert!(result[1].is_empty());
    }

    #[test]
    fn test_filter_targets_with_superset_empty_recursive() {
        let recursive: Vec<Vec<(&BuckTarget, ImpactTraceData)>> = vec![];

        let result =
            filter_targets_with_superset("cell//pkg:foo\ncell//pkg:bar".to_string(), recursive);

        // Empty recursive should return empty result
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_targets_with_superset_all_match() {
        let target1 = create_test_target("foo", "cell//pkg");
        let target2 = create_test_target("bar", "cell//pkg");
        let recursive = vec![
            vec![(&target1, ImpactTraceData::testing())],
            vec![(&target2, ImpactTraceData::testing())],
        ];

        let superset_content = "cell//pkg:foo\ncell//pkg:bar".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // All targets should be included
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 1);
        assert_eq!(result[1].len(), 1);
        assert_eq!(result[0][0].0.name.as_str(), "foo");
        assert_eq!(result[1][0].0.name.as_str(), "bar");
    }

    #[test]
    fn test_filter_targets_with_superset_partial_match() {
        let target1 = create_test_target("foo", "cell//pkg");
        let target2 = create_test_target("bar", "cell//pkg");
        let target3 = create_test_target("baz", "cell//pkg");
        let recursive = vec![
            vec![
                (&target1, ImpactTraceData::testing()),
                (&target2, ImpactTraceData::testing()),
            ],
            vec![(&target3, ImpactTraceData::testing())],
        ];

        let superset_content = "cell//pkg:foo\ncell//pkg:baz".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // Only matching targets should be included
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 1); // Only foo matches from first level
        assert_eq!(result[1].len(), 1); // baz matches from second level
        assert_eq!(result[0][0].0.name.as_str(), "foo");
        assert_eq!(result[1][0].0.name.as_str(), "baz");
    }

    #[test]
    fn test_filter_targets_with_superset_no_match() {
        let target1 = create_test_target("foo", "cell//pkg");
        let target2 = create_test_target("bar", "cell//pkg");
        let recursive = vec![
            vec![(&target1, ImpactTraceData::testing())],
            vec![(&target2, ImpactTraceData::testing())],
        ];

        let superset_content = "cell//pkg:different\ncell//other:target".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // No targets should match
        assert_eq!(result.len(), 2);
        assert!(result[0].is_empty());
        assert!(result[1].is_empty());
    }

    #[test]
    fn test_filter_targets_with_superset_whitespace_handling() {
        let target1 = create_test_target("foo", "cell//pkg");
        let target2 = create_test_target("bar", "cell//pkg");
        let recursive = vec![
            vec![(&target1, ImpactTraceData::testing())],
            vec![(&target2, ImpactTraceData::testing())],
        ];

        // Test with leading/trailing whitespace and empty lines
        let superset_content = "  cell//pkg:foo  \n\n  cell//pkg:bar\n\n".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // Should handle whitespace correctly
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 1);
        assert_eq!(result[1].len(), 1);
        assert_eq!(result[0][0].0.name.as_str(), "foo");
        assert_eq!(result[1][0].0.name.as_str(), "bar");
    }

    #[test]
    fn test_filter_targets_with_superset_multiple_targets_per_level() {
        let target1 = create_test_target("foo", "cell//pkg");
        let target2 = create_test_target("bar", "cell//pkg");
        let target3 = create_test_target("baz", "cell//pkg");
        let target4 = create_test_target("qux", "cell//pkg");
        let recursive = vec![
            vec![
                (&target1, ImpactTraceData::testing()),
                (&target2, ImpactTraceData::testing()),
                (&target3, ImpactTraceData::testing()),
            ],
            vec![(&target4, ImpactTraceData::testing())],
        ];

        let superset_content = "cell//pkg:foo\ncell//pkg:baz\ncell//pkg:qux".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // Should filter correctly within each level
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 2); // foo and baz match
        assert_eq!(result[1].len(), 1); // qux matches

        let level0_names: Vec<&str> = result[0].iter().map(|(t, _)| t.name.as_str()).collect();
        assert!(level0_names.contains(&"foo"));
        assert!(level0_names.contains(&"baz"));
        assert!(!level0_names.contains(&"bar"));

        assert_eq!(result[1][0].0.name.as_str(), "qux");
    }

    #[test]
    fn test_filter_targets_with_superset_different_packages() {
        let target1 = create_test_target("foo", "cell//pkg1");
        let target2 = create_test_target("bar", "cell//pkg2");
        let target3 = create_test_target("baz", "cell//pkg1");
        let recursive = vec![
            vec![
                (&target1, ImpactTraceData::testing()),
                (&target2, ImpactTraceData::testing()),
            ],
            vec![(&target3, ImpactTraceData::testing())],
        ];

        let superset_content = "cell//pkg1:foo\ncell//pkg1:baz".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // Should match based on full target label including package
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].len(), 1); // Only pkg1:foo matches
        assert_eq!(result[1].len(), 1); // pkg1:baz matches
        assert_eq!(result[0][0].0.name.as_str(), "foo");
        assert_eq!(result[0][0].0.package.as_str(), "cell//pkg1");
        assert_eq!(result[1][0].0.name.as_str(), "baz");
    }

    #[test]
    fn test_filter_targets_with_superset_preserves_impact_trace_data() {
        let target = create_test_target("foo", "cell//pkg");
        let impact_data = ImpactTraceData::testing();
        let recursive = vec![vec![(&target, impact_data.clone())]];

        let superset_content = "cell//pkg:foo".to_string();
        let result = filter_targets_with_superset(superset_content, recursive);

        // Should preserve the impact trace data
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 1);
        assert_eq!(result[0][0].1, impact_data);
    }
}
