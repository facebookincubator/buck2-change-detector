/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Utilities for deserializing and flattening Buck `select()` expressions.
//!
//! This module provides `FlattenedSelect<T>`, a wrapper type that deserializes
//! either a plain array or a `select()` expression, flattening all branches
//! into a single `Vec<T>`. This is useful for fields like `tests` where you
//! need all possible values regardless of the configuration.

use std::fmt;

use serde::Deserialize;
use serde::de::MapAccess;
use serde::de::Visitor;

use crate::select::Select;
use crate::select::Visit;

/// A wrapper type that deserializes a `Vec<T>` and flattens select expressions.
///
/// For `select()` expressions, flattens all branches into a single `Vec`.
/// Supports nested select expressions by recursively deserializing branches.
///
/// # Example
///
/// Given a Buck target with nested selects:
/// ```json
/// {
///   "__type": "selector",
///   "entries": {
///     "DEFAULT": {
///       "__type": "selector",
///       "entries": {
///         "DEFAULT": ["test1", "test2"],
///         "config//mode:debug": ["test3"]
///       }
///     },
///     "config//os:linux": ["test4"]
///   }
/// }
/// ```
///
/// `FlattenedSelect<String>` will deserialize this to `["test1", "test2", "test3", "test4"]`.
pub struct FlattenedSelect<T>(pub Vec<T>);

impl<'de, T> Visitor<'de> for Visit<FlattenedSelect<T>>
where
    T: Deserialize<'de>,
{
    type Value = FlattenedSelect<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an array or select expression")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // Use FlattenedSelect<T> recursively to handle nested selects.
        // Each branch can be either a direct array or another select expression.
        let items = Select::<FlattenedSelect<T>>::visit_map(map)?.into_inner();
        let mut res = Vec::new();
        for flattened in items {
            res.extend(flattened.0);
        }
        Ok(FlattenedSelect(res))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut res = match seq.size_hint() {
            None => Vec::new(),
            Some(size) => Vec::with_capacity(size),
        };
        while let Some(x) = seq.next_element::<T>()? {
            res.push(x);
        }
        Ok(FlattenedSelect(res))
    }
}

impl<'de, T> Deserialize<'de> for FlattenedSelect<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(Visit::<Self>::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flattened_select_array() {
        let json = serde_json::json!(["a", "b", "c"]);
        let result: FlattenedSelect<String> =
            serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(result.0, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_flattened_select_simple_select() {
        let json = serde_json::json!({
            "__type": "selector",
            "entries": {
                "DEFAULT": ["a", "b"],
                "config//os:linux": ["c"]
            }
        });
        let result: FlattenedSelect<String> =
            serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(result.0.len(), 3);
        assert!(result.0.contains(&"a".to_string()));
        assert!(result.0.contains(&"b".to_string()));
        assert!(result.0.contains(&"c".to_string()));
    }

    #[test]
    fn test_flattened_select_nested_select() {
        let json = serde_json::json!({
            "__type": "selector",
            "entries": {
                "DEFAULT": {
                    "__type": "selector",
                    "entries": {
                        "DEFAULT": ["inner1", "inner2"],
                        "config//mode:debug": ["inner3"]
                    }
                },
                "config//os:linux": ["outer1"]
            }
        });
        let result: FlattenedSelect<String> =
            serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(result.0.len(), 4);
        assert!(result.0.contains(&"inner1".to_string()));
        assert!(result.0.contains(&"inner2".to_string()));
        assert!(result.0.contains(&"inner3".to_string()));
        assert!(result.0.contains(&"outer1".to_string()));
    }

    #[test]
    fn test_flattened_select_concat() {
        let json = serde_json::json!({
            "__type": "concat",
            "items": [
                ["a", "b"],
                ["c"]
            ]
        });
        let result: FlattenedSelect<String> =
            serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(result.0, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_flattened_select_empty_branch() {
        let json = serde_json::json!({
            "__type": "selector",
            "entries": {
                "DEFAULT": ["a"],
                "config//os:macos": []
            }
        });
        let result: FlattenedSelect<String> =
            serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(result.0, vec!["a"]);
    }
}
