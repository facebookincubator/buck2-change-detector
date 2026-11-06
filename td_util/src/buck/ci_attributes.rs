/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! CI targets and their CI attributes.
//!
//! A CI target is a Buck target that does not build code: it declares a CI
//! job, for example a workflow to run or a shell test, and its attributes
//! (the CI attributes) describe that job. A CI job scheduler reads these
//! attributes for the CI targets that target determination selects, and the
//! target graph stores them so that the scheduler need not query Buck again.

use std::collections::BTreeMap;

use anyhow::Context as _;
use serde::Deserialize;
use serde::Serialize;
use serde::de::Error as _;
use serde_json::Value;
use serde_json::value::RawValue;

/// The rule type of a CI target that runs a workflow.
pub const CI_SKYCASTLE: &str = "ci_skycastle";
/// The rule type of a CI target that runs a job command.
pub const CI_SANDCASTLE: &str = "ci_sandcastle";
/// The rule type of a CI target that runs a shell test.
pub const CI_SHELL_TEST: &str = "ci_shell_test";

/// The rule types of CI targets.
pub const CI_RULE_TYPES: [&str; 3] = [CI_SKYCASTLE, CI_SANDCASTLE, CI_SHELL_TEST];

/// Whether `rule_type`, a short rule type name, is that of a CI target.
pub fn is_ci_rule_type(rule_type: &str) -> bool {
    CI_RULE_TYPES.contains(&rule_type)
}

/// The CI attributes of a CI target.
pub const CI_ATTRIBUTES: [&str; 6] = [
    "args",
    "artifact_flags",
    "workflow_deps",
    "ci_diff_gk",
    "ci_diff_source_predicate",
    "ci_phabricator_diff_tags",
];

/// A `buck2 targets --output-attribute` regex that matches `CI_ATTRIBUTES`.
pub fn ci_attributes_regex() -> String {
    CI_ATTRIBUTES
        .iter()
        .map(|attribute| format!("^{attribute}$"))
        .collect::<Vec<_>>()
        .join("|")
}

/// The present CI attributes of one CI target, as a JSON object.
///
/// `from_attributes` and deserialization make the canonical text of the
/// object: compact, with sorted keys. Two values are therefore equal exactly
/// when their text is equal. The graph writer writes only such values, so
/// reading a stored graph row checks only that the row is a JSON object that
/// `Value` reads, with no whitespace around it.
#[derive(Clone, Debug)]
pub struct CiAttributesJson(Box<RawValue>);

impl CiAttributesJson {
    /// The present attributes of a target as one object, or `None` when no
    /// attribute is present.
    pub fn from_attributes<'a>(
        attributes: impl IntoIterator<Item = (&'a str, &'a Value)>,
    ) -> Option<Self> {
        let object: BTreeMap<&str, &Value> = attributes.into_iter().collect();
        (!object.is_empty()).then(|| Self::from_object(&object))
    }

    /// The canonical text of `object`. Both callers pass a map with sorted
    /// string keys, and such a map always serializes.
    fn from_object(object: &impl Serialize) -> Self {
        Self(serde_json::value::to_raw_value(object).expect("a JSON object serializes"))
    }

    pub fn as_str(&self) -> &str {
        self.0.get()
    }

    /// Checks that `bytes` hold UTF-8 text with one JSON object that `Value`
    /// reads, as a stored graph row must: the readers of a row decode it as
    /// UTF-8, and readers of the target output parse it with `Value`. The
    /// object must fill the row, because `from_validated` drops whitespace
    /// around it: the graph compares a stored row by its text and by its
    /// decoded value, and the two comparisons must agree.
    pub(crate) fn validate(bytes: &[u8]) -> anyhow::Result<()> {
        let text = std::str::from_utf8(bytes).context("CI attributes are not UTF-8 text")?;
        let value: Value = serde_json::from_str(text).context("CI attributes are not JSON")?;
        anyhow::ensure!(value.is_object(), "CI attributes are not a JSON object");
        anyhow::ensure!(
            text.trim_ascii().len() == text.len(),
            "CI attributes have whitespace around the JSON object"
        );
        Ok(())
    }

    /// A value from a graph row that `validate` accepted.
    pub(crate) fn from_validated(text: &str) -> Self {
        Self(RawValue::from_string(text.to_owned()).expect("validated CI attributes"))
    }
}

impl PartialEq for CiAttributesJson {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for CiAttributesJson {}

impl PartialOrd for CiAttributesJson {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CiAttributesJson {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

/// Writes the raw JSON text. Only a JSON serializer writes it as JSON, so the
/// type is for JSON output.
impl Serialize for CiAttributesJson {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CiAttributesJson {
    /// Goes through `Value`, so that it reads JSON text and a
    /// `serde_json::Value`, and stores the canonical text.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Object(object) => Ok(Self::from_object(&object)),
            _ => Err(D::Error::custom("CI attributes are not a JSON object")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn attributes_are_canonical_text() {
        let workflow_deps = json!(["b"]);
        let args = json!({"z": 1, "a": [2, 1]});
        let a =
            CiAttributesJson::from_attributes([("workflow_deps", &workflow_deps), ("args", &args)])
                .unwrap();
        let b: CiAttributesJson =
            serde_json::from_str(r#"{ "args": {"a": [2, 1], "z": 1}, "workflow_deps": ["b"] }"#)
                .unwrap();

        assert_eq!(
            a.as_str(),
            r#"{"args":{"a":[2,1],"z":1},"workflow_deps":["b"]}"#
        );
        assert_eq!(a, b);
    }

    #[test]
    fn no_present_attribute_gives_none() {
        assert_eq!(CiAttributesJson::from_attributes(std::iter::empty()), None);
    }

    #[test]
    fn deserialization_rejects_values_that_are_not_objects() {
        assert!(serde_json::from_str::<CiAttributesJson>("[1]").is_err());
        assert!(serde_json::from_value::<CiAttributesJson>(json!("text")).is_err());
    }

    #[test]
    fn deserializes_from_a_json_value() {
        let value: CiAttributesJson = serde_json::from_value(json!({"ci_diff_gk": "gk"})).unwrap();
        assert_eq!(value.as_str(), r#"{"ci_diff_gk":"gk"}"#);
    }

    #[test]
    fn serializes_as_raw_json() {
        let value: CiAttributesJson = serde_json::from_str(r#"{"args":{"a":1}}"#).unwrap();
        assert_eq!(
            serde_json::to_string(&json!({"ci_attributes": value})).unwrap(),
            r#"{"ci_attributes":{"args":{"a":1}}}"#
        );
    }

    /// A stored row is valid exactly when `Value` reads it as an object and
    /// the object fills the row.
    #[test]
    fn validation_accepts_exactly_the_unpadded_objects_that_value_reads() {
        let rows: [&[u8]; 15] = [
            br#"{"args":{"a":[1,{"b":null}]}}"#,
            b"{\"args\":\"\\u00e9 \\ud83d\\ude00\"}",
            br#"{"n":[-1,2.5e3,18446744073709551615]}"#,
            br#"{"args":"#,
            br#"{"args":{}} {}"#,
            br#"{"args":[1,]}"#,
            b"[]",
            br#""text""#,
            b"{\"args\":\"\xff\"}",
            b"{\"\xff\":1}",
            b"{\"args\":\"\\ud800\"}",
            br#"{"args":1e400}"#,
            b"{\"args\":\"\\x\"}",
            br#" {"args":{}}"#,
            b"{\"args\":{}}\n",
        ];
        let mut accepted = 0;
        for row in rows {
            let value_reads_an_object =
                serde_json::from_slice::<Value>(row).is_ok_and(|value| value.is_object());
            let object_fills_the_row = row.trim_ascii().len() == row.len();
            let valid = CiAttributesJson::validate(row).is_ok();
            assert_eq!(
                valid,
                value_reads_an_object && object_fills_the_row,
                "{}",
                String::from_utf8_lossy(row)
            );
            accepted += usize::from(valid);
        }
        assert_eq!(accepted, 3);
    }

    #[test]
    fn the_regex_matches_exactly_the_ci_attributes() {
        let regex = regex::Regex::new(&ci_attributes_regex()).unwrap();
        for attribute in CI_ATTRIBUTES {
            assert!(regex.is_match(attribute), "{attribute}");
        }
        assert!(!regex.is_match("args_extra"));
        assert!(!regex.is_match("labels"));
    }
}
