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
pub fn cell_build_file(cell: &str) -> &'static str {
    if cell == "fbcode" || cell == "prelude" || cell == "toolchains" {
        "TARGETS"
    } else {
        "BUCK"
    }
}
