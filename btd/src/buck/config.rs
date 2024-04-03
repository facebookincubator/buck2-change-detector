/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Configuration that we hardcode, because parsing it is too expensive.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_buck_deployment() {
        assert!(is_buck_deployment(&CellPath::new(
            "fbsource//tools/buck2-versions/previous"
        )));
        assert!(is_buck_deployment(&CellPath::new(
            "fbsource//tools/buck2-versions/stable"
        )));
    }
}
