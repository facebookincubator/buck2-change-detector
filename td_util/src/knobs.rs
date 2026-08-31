/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#[cfg(all(fbcode_build, target_os = "linux"))]
use crate::external_calls_cache::CacheType;
#[cfg(all(fbcode_build, target_os = "linux"))]
use crate::external_calls_cache::call_cached_sync;

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_boolean_knob(_name: &str) -> bool {
    false
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_boolean_knob(name: &str) -> bool {
    eval_cached(name, None, None).unwrap_or(false)
}

#[cfg(all(fbcode_build, target_os = "linux"))]
enum KnobValueType {
    Boolean,
    Integer,
}

#[cfg(all(fbcode_build, target_os = "linux"))]
fn cache_key(
    value_type: KnobValueType,
    name: &str,
    hash_val: Option<&str>,
    switch_val: Option<&str>,
) -> String {
    let value_type = match value_type {
        KnobValueType::Boolean => "bool",
        KnobValueType::Integer => "i64",
    };
    serde_json::to_string(&(value_type, name, hash_val, switch_val))
        .expect("should be unreachable: a tuple of string slices always serializes")
}

/// Records the raw outcome rather than the caller's default, so two call sites
/// reading one knob with different defaults cannot contaminate each other
/// through a shared entry.
///
/// An absent knob is recorded as `None`, which is the stable raw outcome that
/// every caller here already maps to its own default. Recording that outcome
/// keeps a replay from treating the absence as a cache miss.
#[cfg(all(fbcode_build, target_os = "linux"))]
fn eval_cached(name: &str, hash_val: Option<&str>, switch_val: Option<&str>) -> Option<bool> {
    call_cached_sync(
        CacheType::JustKnobs,
        || cache_key(KnobValueType::Boolean, name, hash_val, switch_val),
        // @lint-ignore JUSTKNOBSUNSAFEUSAGE generic accessor; call sites pass literal names
        || justknobs::eval(name, hash_val, switch_val).ok(),
    )
}

#[cfg(all(fbcode_build, target_os = "linux"))]
fn get_cached(name: &str, switch_val: Option<&str>) -> Option<i64> {
    call_cached_sync(
        CacheType::JustKnobs,
        || cache_key(KnobValueType::Integer, name, None, switch_val),
        // @lint-ignore JUSTKNOBSUNSAFEUSAGE generic accessor; call sites pass literal names
        || justknobs::get(name, switch_val).ok(),
    )
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
    eval_cached(name, None, switch_val).unwrap_or(default)
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
    eval_cached(name, hash_val, switch_val).unwrap_or(default)
}

#[cfg(any(not(fbcode_build), not(target_os = "linux")))]
pub fn check_integer_knob(_name: &str, default_value: i64) -> i64 {
    default_value
}

#[cfg(all(fbcode_build, target_os = "linux"))]
pub fn check_integer_knob(name: &str, default_value: i64) -> i64 {
    get_cached(name, None).unwrap_or(default_value)
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
    get_cached(name, switch_val).unwrap_or(default_value)
}

#[cfg(all(test, fbcode_build, target_os = "linux"))]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::bool_no_hash_or_switch(
        KnobValueType::Boolean,
        "knob",
        None,
        None,
        r#"["bool","knob",null,null]"#
    )]
    #[case::bool_hash_and_switch(
        KnobValueType::Boolean,
        "knob",
        Some("hash"),
        Some("switch"),
        r#"["bool","knob","hash","switch"]"#
    )]
    #[case::int_switch_only(
        KnobValueType::Integer,
        "knob",
        None,
        Some("switch"),
        r#"["i64","knob",null,"switch"]"#
    )]
    #[case::int_hash_and_switch(
        KnobValueType::Integer,
        "knob",
        Some("hash"),
        Some("switch"),
        r#"["i64","knob","hash","switch"]"#
    )]
    #[case::empty_hash_is_not_absent(
        KnobValueType::Boolean,
        "knob",
        Some(""),
        Some("switch"),
        r#"["bool","knob","","switch"]"#
    )]
    #[case::delimiter_in_name(
        KnobValueType::Boolean,
        "a|",
        Some("b"),
        Some("c"),
        r#"["bool","a|","b","c"]"#
    )]
    #[case::delimiter_in_switch(
        KnobValueType::Boolean,
        "a",
        None,
        Some("b|c"),
        r#"["bool","a",null,"b|c"]"#
    )]
    fn cache_key_captures_every_field(
        #[case] value_type: KnobValueType,
        #[case] name: &str,
        #[case] hash_val: Option<&str>,
        #[case] switch_val: Option<&str>,
        #[case] expected: &str,
    ) {
        assert_eq!(cache_key(value_type, name, hash_val, switch_val), expected);
    }
}
