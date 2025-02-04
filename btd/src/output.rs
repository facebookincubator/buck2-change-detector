/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt;
use std::fmt::Display;

use serde::Serialize;

use crate::buck::labels::Labels;
use crate::buck::targets::BuckTarget;
use crate::buck::types::Oncall;
use crate::buck::types::TargetLabel;
use crate::diff::ImpactTraceData;

#[derive(Debug, Serialize)]
pub struct Output<'a> {
    target: TargetLabel,
    #[serde(rename = "type")]
    typ: &'a str,
    oncall: &'a Option<Oncall>,
    depth: u64,
    labels: Labels,
    reason: ImpactTraceData,
}

impl<'a> Output<'a> {
    pub fn from_target(
        x: &'a BuckTarget,
        depth: u64,
        uses_sudo: bool,
        reason: ImpactTraceData,
    ) -> Self {
        let additional_labels = if uses_sudo && !x.labels.contains("uses_sudo") {
            Labels::new(&["uses_sudo"])
        } else {
            Labels::default()
        };
        Self {
            target: x.label(),
            typ: x.rule_type.short(),
            oncall: &x.oncall,
            depth,
            // package values must come before target labels for overrides to work.
            labels: x
                .package_values
                .labels
                .merge3(&x.labels, &additional_labels),
            reason,
        }
    }
}

impl<'a> Display for Output<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", serde_json::to_string(self).unwrap())
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum OutputFormat {
    Text,
    Json,
    JsonLines,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::buck::types::CellPath;
    use crate::buck::types::PackageValues;
    use crate::buck::types::TargetHash;
    use crate::diff::RootImpactKind;

    #[test]
    fn test_read_targets() {
        let json = serde_json::json!(
            {
                "target": "fbcode//me:test",
                "type": "python_library",
                "depth": 3,
                "labels": ["my_label", "another_label"],
                "oncall": "my_team",
                "reason": {
                    "affected_dep": "cell//foo:bar",
                    "is_terminal": false,
                    "root_cause": ["fbcode//me:test", "inputs"],
                }
            }
        );

        let target = BuckTarget {
            deps: Box::new([
                TargetLabel::new("toolchains//:python"),
                TargetLabel::new("fbcode//python:library"),
            ]),
            inputs: Box::new([CellPath::new("fbcode//me/file.bzl")]),
            hash: TargetHash::new("43ce1a7a56f10225413a2991febb853a"),
            labels: Labels::new(&["my_label", "another_label"]),
            oncall: Some(Oncall::new("my_team")),
            ..BuckTarget::testing("test", "fbcode//me", "prelude//rules.bzl:python_library")
        };
        let output = Output::from_target(
            &target,
            3,
            false,
            ImpactTraceData {
                root_cause: ("fbcode//me:test".to_owned(), RootImpactKind::Inputs),
                ..ImpactTraceData::testing()
            },
        );
        assert_eq!(serde_json::to_value(&output).unwrap(), json);
        assert_eq!(
            serde_json::from_str::<Value>(&output.to_string()).unwrap(),
            json
        );
        assert!(!output.to_string().contains('\n'));

        let target_no_oncall = BuckTarget {
            oncall: None,
            ..target
        };
        let json_no_oncall = serde_json::json!(
            {
                "target": "fbcode//me:test",
                "type": "python_library",
                "depth": 3,
                "labels": ["my_label", "another_label"],
                "oncall": Value::Null,
                "reason": ImpactTraceData::testing(),
            }
        );
        assert_eq!(
            serde_json::to_value(Output::from_target(
                &target_no_oncall,
                3,
                false,
                ImpactTraceData::testing(),
            ))
            .unwrap(),
            json_no_oncall
        );
    }

    #[test]
    fn test_label_ordering() {
        let target = BuckTarget {
            labels: Labels::new(&["target_label"]),
            package_values: PackageValues::new(&["must-come-first"]),
            ..BuckTarget::testing("test", "fbcode//me", "prelude//rules.bzl:python_library")
        };
        let output = Output::from_target(&target, 3, false, ImpactTraceData::testing());
        assert_eq!(
            output.labels,
            Labels::new(&["must-come-first", "target_label"])
        );
    }
}
