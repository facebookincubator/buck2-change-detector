/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;

use serde::Deserialize;
use serde::Serialize;
use serde::de::Error;
use serde::de::MapAccess;
use serde::de::Visitor;
use td_util::string::InternString;

/// A set of labels
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct Labels(Box<[InternString]>);

impl Labels {
    pub fn new(labels: &[&str]) -> Self {
        Self(labels.iter().map(|x| InternString::new(x)).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, label: &str) -> bool {
        self.0.contains(&InternString::new(label))
    }

    pub fn merge(&self, other: &Labels) -> Self {
        Self(self.0.iter().chain(other.0.iter()).cloned().collect())
    }

    pub fn merge3(&self, other: &Labels, third: &Labels) -> Self {
        Self(
            self.0
                .iter()
                .chain(other.0.iter())
                .chain(third.0.iter())
                .cloned()
                .collect(),
        )
    }
}

impl Deref for Labels {
    type Target = [InternString];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for Labels {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

/// A label is really a list of possible alternative labels in different universes (because of select).
/// The difference to Labels is that concat merges strings rather than Vec
struct Label<'a>(Vec<Cow<'a, str>>);

struct SelectEntries<T>(Vec<T>);

enum Select<T> {
    Selector(Vec<T>),
    Concat(Vec<T>),
}

struct Visit<T>(PhantomData<T>);

impl<T> Visit<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

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
        // We have the entries {key1: value1, ...}
        // We return the values, concattenated
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

impl<'de, T> Select<T>
where
    T: Deserialize<'de>,
{
    fn visit_map<A>(mut map: A) -> Result<Self, A::Error>
    where
        A: MapAccess<'de>,
    {
        // We expect one of:
        //   {"__type":"selector", "entries": {key1: value1, ...}}
        //   {"__type":"concat", "items": [value1, ..]}

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

impl<'de> Visitor<'de> for Visit<Label<'de>> {
    type Value = Label<'de>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a label (potentially with select's)")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        match Select::<Label>::visit_map(map)? {
            Select::Selector(xs) => Ok(Label(xs.into_iter().flat_map(|x| x.0).collect())),
            Select::Concat(xs) => {
                // We are concatenating a series of labels - we don't want to take the cross product (that is O(n^2))
                // As an approximation, we take the first from each label set, and concat those
                let mut res = String::new();
                for x in xs {
                    if let Some(x0) = x.0.first() {
                        res.push_str(x0)
                    }
                }
                Ok(Label(vec![Cow::Owned(res)]))
            }
        }
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: Error,
    {
        Ok(Label(vec![Cow::Owned(v)]))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        self.visit_string(v.to_owned())
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        Ok(Label(vec![Cow::Borrowed(v)]))
    }
}

impl<'de> Deserialize<'de> for Label<'de> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(Visit::<Self>::new())
    }
}

impl<'de> Visitor<'de> for Visit<Labels> {
    type Value = Labels;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a series of labels (potentially with select's in them)")
    }

    fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let lbls = match Select::<Labels>::visit_map(map)? {
            Select::Selector(xs) => xs,
            Select::Concat(xs) => xs,
        };
        let mut res = Vec::new();
        for xs in lbls {
            for x in xs.0.iter() {
                res.push(x.clone());
            }
        }
        Ok(Labels(res.into_boxed_slice()))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut res = match seq.size_hint() {
            None => Vec::new(),
            Some(size) => Vec::with_capacity(size),
        };
        while let Some(x) = seq.next_element::<Label>()? {
            res.extend(x.0.iter().map(|x| InternString::new(x)))
        }
        Ok(Labels(res.into_boxed_slice()))
    }
}

impl<'de> Deserialize<'de> for Labels {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(Visit::<Self>::new())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[test]
    fn test_labels() {
        fn testcase(json: Value, expected: &[&str]) {
            // We deliberately avoid serde_json::from_value to more accurately replicate what we get
            let result: Labels =
                serde_json::from_str(&serde_json::to_string(&json).unwrap()).unwrap();
            assert_eq!(
                &result.0.iter().map(|x| x.as_str()).collect::<Vec<_>>(),
                expected
            );
        }

        testcase(
            serde_json::json!(
                [
                    {
                        "__type": "selector",
                        "entries": {
                            "DEFAULT": "c",
                            "ovr_config//os:linux": "a",
                            "ovr_config//os:macos": "b"
                        }
                    },
                    "d",
                    "e",
                    {
                        "__type": "concat",
                        "items": [
                            {
                                "__type": "selector",
                                "entries": {
                                    "DEFAULT": "2",
                                    "ovr_config//os:linux": "1"
                                }
                            },
                            "suffix"
                        ]
                    }
                ]
            ),
            &["c", "a", "b", "d", "e", "2suffix"],
        );
        testcase(
            serde_json::json!(
                {
                    "__type": "selector",
                    "entries": {
                        "DEFAULT": ["c","d"],
                        "ovr_config//os:linux": ["a"],
                        "ovr_config//os:macos": ["b"]
                    }
                }
            ),
            &["c", "d", "a", "b"],
        );
        testcase(
            serde_json::json!({
                "__type": "concat",
                "items": [
                    {
                        "__type": "selector",
                        "entries": {
                            "DEFAULT": ["c"],
                            "ovr_config//os:linux": ["a"]
                        }
                    },
                    ["test", "more"]
                ]
            }),
            &["c", "a", "test", "more"],
        )
    }
}
