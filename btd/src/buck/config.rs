/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Configuration that we hardcode, because parsing it is too expensive.

/// What is the build file for a given cell.
/// Use `&str` rather than `CellName` since it is cheaper to construct from an existing string.
pub fn cell_build_files(cell: &str) -> &'static [&'static str] {
    if cell == "fbcode" || cell == "prelude" || cell == "toolchains" {
        &["TARGETS.v2", "TARGETS", "BUCK.v2", "BUCK"]
    } else {
        &["BUCK.v2", "BUCK"]
    }
}

/// Certain bzl files should be excluded from transitive impact tracing.
/// This list should *only* be extended if we are certain that changes to the file
/// will have its impact traced through other means, e.g., attribute changes.
pub fn should_exclude_bzl_file_from_transitive_impact_tracing(path: &str) -> bool {
    path.ends_with(".bzl")
        && ["fbcode//target_determinator/macros"]
            .iter()
            .any(|prefix| path.starts_with(*prefix))
}
