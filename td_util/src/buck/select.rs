/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Common types for deserializing Buck `select()` expressions.
//!
//! This module provides shared infrastructure for handling Buck's `select()`
//! expressions during deserialization. These types are used by both `labels.rs`
//! and `flattened_select.rs`.

use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::Error;
use serde::de::MapAccess;
use serde::de::Visitor;

/// A helper struct for implementing serde Visitor patterns.
///
/// This is a zero-sized type that carries the target type as a phantom type parameter.
/// It's used to implement `Visitor` for different target types.
pub struct Visit<T>(PhantomData<T>);

impl<T> Visit<T> {
    pub fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> Default for Visit<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Represents the entries of a select expression: `{key1: value1, key2: value2, ...}`.
///
/// When deserializing, this discards the keys and collects only the values.
pub struct SelectEntries<T>(pub Vec<T>);

impl<'de, T> Visitor<'de> for Visit<SelectEntries<T>>
where
    T: Deserialize<'de>,
{
    type Value = SelectEntries<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("the entries map of a select-defined block")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut res = match map.size_hint() {
            None => Vec::new(),
            Some(size) => Vec::with_capacity(size),
        };
        while let Some((_, x)) = map.next_entry::<&str, T>()? {
            res.push(x);
        }
        Ok(SelectEntries(res))
    }
}

impl<'de, T> Deserialize<'de> for SelectEntries<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(Visit::<Self>::new())
    }
}

/// Represents a Buck `select()` expression or concatenation.
///
/// A select expression has one of two forms in the serialized JSON:
/// - `{"__type":"selector", "entries": {key1: value1, ...}}`
/// - `{"__type":"concat", "items": [value1, ..]}`
///
/// The `Selector` variant contains all branch values from a select expression.
/// The `Concat` variant contains all items from a concatenation expression.
pub enum Select<T> {
    Selector(Vec<T>),
    Concat(Vec<T>),
}

impl<T> Select<T> {
    /// Consume the select and return the inner values regardless of variant.
    pub fn into_inner(self) -> Vec<T> {
        match self {
            Select::Selector(xs) => xs,
            Select::Concat(xs) => xs,
        }
    }
}

impl<'de, T> Select<T>
where
    T: Deserialize<'de>,
{
    /// Parse a select expression from a map access.
    ///
    /// This is called when the deserializer encounters a JSON object that
    /// represents a select expression.
    pub fn visit_map<A>(mut map: A) -> Result<Self, A::Error>
    where
        A: MapAccess<'de>,
    {
        let check = |b, msg| {
            if b {
                Ok(())
            } else {
                Err(A::Error::custom(msg))
            }
        };
        check(
            map.next_key::<&str>()? == Some("__type"),
            "expecting a select with a `__type` key",
        )?;
        let res = match map.next_value::<&str>()? {
            "selector" => {
                check(
                    map.next_key::<&str>()? == Some("entries"),
                    "expected an entries key",
                )?;
                let res = map.next_value::<SelectEntries<T>>()?;
                Select::Selector(res.0)
            }
            "concat" => {
                check(
                    map.next_key::<&str>()? == Some("items"),
                    "expected an items key",
                )?;
                let res = map.next_value::<Vec<T>>()?;
                Select::Concat(res)
            }
            typ => {
                return Err(A::Error::custom(format!(
                    "expecting a `__type` of selector or concat, got `{}`",
                    typ
                )));
            }
        };
        check(map.next_key::<&str>()?.is_none(), "expected no more keys")?;
        Ok(res)
    }
}
