/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use serde::Deserialize;
use serde::Deserializer;

use crate::flattened_select::FlattenedSelect;
use crate::types::TargetLabel;

/// Deserializer for `tests` attribute that handles both simple arrays and select() expressions.
/// For select() expressions, flattens all branches into a single array.
///
/// If deserialization fails (e.g., due to an unexpected format), logs a warning and returns
/// an empty array instead of failing the entire parse. This ensures BTD continues to progress
pub fn deserialize_tests<'de, D>(deserializer: D) -> Result<Box<[TargetLabel]>, D::Error>
where
    D: Deserializer<'de>,
{
    // First, deserialize the raw JSON value so we can attempt our custom deserialization
    // without consuming the deserializer on failure.
    let value = serde_json::Value::deserialize(deserializer)?;

    match FlattenedSelect::<TargetLabel>::deserialize(&value) {
        Ok(flattened) => Ok(flattened.0.into_boxed_slice()),
        Err(e) => {
            // Log the error but don't fail - return empty array to allow BTD to continue
            tracing::warn!(
                "Failed to deserialize 'tests' attribute, returning empty array: {}",
                e
            );
            Ok(Box::new([]))
        }
    }
}
