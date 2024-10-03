/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Configuration that we hardcode, because parsing it is too expensive.

use super::targets::BuckTarget;
use crate::buck::types::CellPath;

/// Certain bzl files should be excluded from transitive impact tracing.
/// This list should *only* be extended if we are certain that changes to the file
/// will have its impact traced through other means, e.g., attribute changes.
pub fn should_exclude_bzl_file_from_transitive_impact_tracing(path: &str) -> bool {
    path.ends_with(".bzl")
        && ["fbcode//target_determinator/macros"]
            .iter()
            .any(|prefix| path.starts_with(*prefix))
}

pub fn is_buck_deployment(path: &CellPath) -> bool {
    path.as_str().starts_with("fbsource//tools/buck2-versions/")
}

// Touching these files will let btd treat everything as affected.
pub fn is_buckconfig_change(path: &CellPath) -> bool {
    let ext = path.extension();
    let str = path.as_str();
    // Don't treat changes to buck2 tests as buckconfig changes, otherwise we run way too much CI on
    // these
    if str.contains("buck2/tests") {
        return false;
    }
    ext == Some("buckconfig")
        || str.starts_with("fbsource//tools/buckconfigs/")
        || (ext.is_none()
            && (str.starts_with("fbsource//arvr/mode/")
                || str.starts_with("fbsource//fbcode/mode/")))
}

pub fn is_target_with_buck_dependencies(buck_target: &BuckTarget) -> bool {
    let dependency_checked_rule_types = ["ci_translator_workflow"];

    if dependency_checked_rule_types.contains(&buck_target.rule_type.short()) {
        !buck_target.ci_deps.is_empty()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buck::labels::Labels;
    use crate::buck::types::Package;
    use crate::buck::types::PackageValues;
    use crate::buck::types::RuleType;
    use crate::buck::types::TargetHash;
    use crate::buck::types::TargetName;
    use crate::buck::types::TargetPattern;

    #[test]
    fn test_is_buck_deployment() {
        assert!(is_buck_deployment(&CellPath::new(
            "fbsource//tools/buck2-versions/previous"
        )));
        assert!(is_buck_deployment(&CellPath::new(
            "fbsource//tools/buck2-versions/stable"
        )));
    }

    #[test]
    fn test_is_buckconfig_change() {
        // random buckconfigs
        assert!(!is_buckconfig_change(&CellPath::new(
            "fbcode//some_config.bcfg"
        )));
        // buckconfigs
        assert!(is_buckconfig_change(&CellPath::new(
            "fbsource//.buckconfig"
        )));
        // bcfg
        assert!(is_buckconfig_change(&CellPath::new(
            "fbsource//tools/buckconfigs/abc/xyz.bcfg"
        )));
        assert!(!is_buckconfig_change(&CellPath::new(
            "fbcode//buck2/TARGETS"
        )));
        assert!(!is_buckconfig_change(&CellPath::new(
            "fbcode//buck2/src/file.rs"
        )));
        assert!(is_buckconfig_change(&CellPath::new(
            "fbsource//tools/buckconfigs/cxx/windows/clang.inc"
        )));
        assert!(is_buckconfig_change(&CellPath::new(
            "fbsource//arvr/mode/dv/dev.buckconfig"
        )));
        assert!(is_buckconfig_change(&CellPath::new(
            "fbsource//tools/buckconfigs/fbsource-specific.bcfg"
        )));
        assert!(is_buckconfig_change(&CellPath::new(
            "fbsource//.buckconfig"
        )));
        assert!(!is_buckconfig_change(&CellPath::new(
            "fbcode//buck2/tests/foo_data/.buckconfig"
        )));
    }

    fn run_is_target_with_dependency_test(
        rule_types: &[&str],
        deps: Option<&[&str]>,
        expected: bool,
    ) {
        fn create_buck_target(rule_type: &str, ci_deps: Option<&[&str]>) -> BuckTarget {
            BuckTarget {
                name: TargetName::new("myTargetName"),
                package: Package::new("myPackage"),
                package_values: PackageValues::default(),
                rule_type: RuleType::new(rule_type),
                oncall: None,
                deps: Box::new([]),
                inputs: Box::new([]),
                hash: TargetHash::new("myTargetHash"),
                labels: Labels::default(),
                ci_srcs: Box::new([]),
                ci_deps: match ci_deps {
                    Some(deps) => deps.iter().map(|&dep| TargetPattern::new(dep)).collect(),
                    None => Box::new([]),
                },
            }
        }

        let test_targets = rule_types
            .iter()
            .map(|&rule_type| create_buck_target(rule_type, deps))
            .collect::<Vec<_>>();

        for target in test_targets {
            assert_eq!(is_target_with_buck_dependencies(&target), expected);
        }
    }

    #[test]
    fn test_is_target_with_buck_dependencies_returns_true_when_deps_are_set() {
        let rule_types = ["ci_translator_workflow"];
        run_is_target_with_dependency_test(&rule_types, Some(&["ci_dep1", "ci_dep2"]), true);
    }

    #[test]
    fn test_is_target_with_buck_dependencies_returns_true_when_deps_are_not_set_and_is_custom_rule_type()
     {
        let rule_types = ["my_custom_rule_type"];
        run_is_target_with_dependency_test(&rule_types, None, true);
    }

    #[test]
    fn test_is_target_with_buck_dependencies_returns_false_when_deps_are_not_set() {
        let rule_types = ["ci_translator_workflow"];
        run_is_target_with_dependency_test(&rule_types, None, false);
    }
}
