/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_boolean_knob(name: &str) -> bool {
    false
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_boolean_knob(name: &str) -> bool {
    justknobs::eval(name, None, None).unwrap_or(false)
}

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_integer_knob(name: &str, default_value: i64) -> i64 {
    default_value
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_integer_knob(name: &str, default_value: i64) -> i64 {
    justknobs::get(name, None).unwrap_or(default_value)
}
