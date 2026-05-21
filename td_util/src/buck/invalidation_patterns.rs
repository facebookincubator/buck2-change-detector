/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Shared classifier of changed file paths that affect buck2 target graph
//! correctness. Consumers: `btd_v2` for rerun-or-not decisions, and
//! `graph_fetch` for cached-graph invalidation classification. Keeping the
//! patterns here ensures the two consumers stay in lockstep.

use crate::types::CellPath;

/// Whether a changed file requires marking all targets as affected.
/// Limited to buckconfig and mode directory changes that alter how every
/// target in the repo is built.
pub fn invalidates_graph(path: &CellPath) -> bool {
    const MODE_DIRECTORIES: &[&str] = &[
        "fbsource//arvr/mode/",
        "fbsource//fbandroid/mode/",
        "fbsource//fbcode/mode/",
        "fbsource//fbobjc/mode/",
        "fbsource//xplat/mode/",
    ];

    let s = path.as_str();

    if s.contains("buck2/tests") {
        return false;
    }

    path.extension() == Some("buckconfig")
        || s.starts_with("fbsource//tools/buckconfigs/")
        || (path.extension().is_none()
            && MODE_DIRECTORIES.iter().any(|prefix| s.starts_with(prefix)))
}

/// Whether a changed file requires re-querying the full universe but can
/// still use normal affected-target analysis. These files alter the build
/// graph structure but their impact can be determined incrementally.
pub fn requires_graph_rerun(path: &CellPath) -> bool {
    let s = path.as_str();
    s.starts_with("fbsource//tools/buck2-versions/")
        || s.to_ascii_lowercase().contains("third-party-buck")
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::buckconfig_at_root("fbsource//.buckconfig", true)]
    #[case::buckconfig_nested("fbcode//some/path/.buckconfig", true)]
    #[case::buckconfig_arvr_mode("fbsource//arvr/mode/dv/dev.buckconfig", true)]
    #[case::bcfg_in_tools_buckconfigs("fbsource//tools/buckconfigs/fbsource-specific.bcfg", true)]
    #[case::bcfg_outside_tools_buckconfigs("fbcode//some_config.bcfg", false)]
    #[case::mode_directory_fbcode("fbsource//fbcode/mode/dev", true)]
    #[case::mode_directory_xplat("fbsource//xplat/mode/opt", true)]
    #[case::mode_directory_arvr("fbsource//arvr/mode/dv/config", true)]
    #[case::mode_directory_fbandroid("fbsource//fbandroid/mode/opt", true)]
    #[case::mode_directory_fbobjc("fbsource//fbobjc/mode/dev", true)]
    #[case::mode_dir_with_extension("fbsource//fbcode/mode/dev.buckconfig", true)]
    #[case::mode_dir_non_buckconfig_ext("fbsource//fbcode/mode/dev.json", false)]
    #[case::mode_dir_wrong_cell("fbcode//video/mode/player", false)]
    #[case::buckconfig_dir_not_invalidating("fbsource//some/buckconfig/file.txt", false)]
    #[case::third_party_buck("fbsource//third-party-buck/platform/build", false)]
    #[case::third_party_buck_uppercase("fbsource//Third-Party-Buck/platform", false)]
    #[case::tools_buckconfigs("fbsource//tools/buckconfigs/cxx/windows/clang.inc", true)]
    #[case::tools_buckconfigs_fbcode_modes("fbsource//tools/buckconfigs/fbcode/modes/opt", true)]
    #[case::buck2_versions("fbsource//tools/buck2-versions/stable", false)]
    #[case::regular_source_file("fbcode//some/path/main.cpp", false)]
    #[case::regular_rust_file("fbcode//target_determinator/btd/src/lib.rs", false)]
    #[case::bzl_file("fbcode//some/defs.bzl", false)]
    #[case::buck_file("fbcode//pkg/BUCK", false)]
    #[case::targets_file("fbcode//pkg/TARGETS", false)]
    #[case::package_file("fbcode//pkg/PACKAGE", false)]
    #[case::buck2_test_buckconfig("fbcode//buck2/tests/foo_data/.buckconfig", false)]
    #[case::buck2_test_bcfg("fbcode//buck2/tests/some_test/config.bcfg", false)]
    fn detects_graph_invalidating_changes(#[case] path: &str, #[case] expected: bool) {
        let cell_path = CellPath::new(path);
        assert_eq!(
            invalidates_graph(&cell_path),
            expected,
            "invalidates_graph({path}) should be {expected}",
        );
    }

    #[rstest]
    #[case::third_party_buck("fbsource//third-party-buck/platform/build", true)]
    #[case::third_party_buck_uppercase("fbsource//Third-Party-Buck/platform", true)]
    #[case::buck2_versions("fbsource//tools/buck2-versions/stable", true)]
    #[case::buckconfig_not_rerun_only("fbsource//.buckconfig", false)]
    #[case::mode_dir_not_rerun_only("fbsource//fbcode/mode/dev", false)]
    #[case::regular_source_file("fbcode//some/path/main.cpp", false)]
    fn detects_graph_rerun_changes(#[case] path: &str, #[case] expected: bool) {
        let cell_path = CellPath::new(path);
        assert_eq!(
            requires_graph_rerun(&cell_path),
            expected,
            "requires_graph_rerun({path}) should be {expected}",
        );
    }
}
