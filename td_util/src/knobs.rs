/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_boolean_knob(_name: &str) -> bool {
    false
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_boolean_knob(name: &str) -> bool {
    justknobs::eval(name, None, None).unwrap_or(false)
}

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_boolean_knob_with_switch(
    _name: &str,
    _switch_val: Option<&str>,
    default: bool,
) -> bool {
    default
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_boolean_knob_with_switch(name: &str, switch_val: Option<&str>, default: bool) -> bool {
    justknobs::eval(name, None, switch_val).unwrap_or(default)
}

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_boolean_knob_with_switch_and_consistent_pass_rate(
    _name: &str,
    _hash_val: Option<&str>,
    _switch_val: Option<&str>,
    default: bool,
) -> bool {
    default
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_boolean_knob_with_switch_and_consistent_pass_rate(
    name: &str,
    hash_val: Option<&str>,
    switch_val: Option<&str>,
    default: bool,
) -> bool {
    justknobs::eval(name, hash_val, switch_val).unwrap_or(default)
}

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_integer_knob(_name: &str, default_value: i64) -> i64 {
    default_value
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_integer_knob(name: &str, default_value: i64) -> i64 {
    justknobs::get(name, None).unwrap_or(default_value)
}

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_integer_knob_with_switch(
    _name: &str,
    _switch_val: Option<&str>,
    default_value: i64,
) -> i64 {
    default_value
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_integer_knob_with_switch(
    name: &str,
    switch_val: Option<&str>,
    default_value: i64,
) -> i64 {
    justknobs::get(name, switch_val).unwrap_or(default_value)
}
