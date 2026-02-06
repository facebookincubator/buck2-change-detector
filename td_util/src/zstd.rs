/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Utilities for working with zstd compressed files.

use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Zstd frame magic number per RFC 8878 Section 3.1.1
/// https://datatracker.ietf.org/doc/html/rfc8878#section-3.1.1
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Check if a file has a zstd extension (.zst)
pub fn is_zstd(filename: &Path) -> bool {
    match filename.extension() {
        Some(x) => x == "zst",
        None => false,
    }
}

/// Check if a file contains zstd-compressed data by reading its magic bytes.
/// Returns true if the file starts with the zstd magic bytes.
pub fn has_zstd_magic(file_path: &Path) -> bool {
    let Ok(mut file) = File::open(file_path) else {
        return false;
    };

    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() {
        magic == ZSTD_MAGIC
    } else {
        false
    }
}
