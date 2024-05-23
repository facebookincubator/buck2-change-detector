/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! The schedule types available.

use std::cmp::Eq;
use std::collections::HashSet;
use std::hash::Hash;

use clap::ValueEnum;
use lazy_static::lazy_static;
use parse_display::Display;
use serde::Deserialize;
use serde::Serialize;

lazy_static! {
    static ref CHANGESET_SCHEDULE_TYPES: HashSet<&'static str> =
        HashSet::from(["diff", "landcastle", "master", "postcommit", "relbranch"]);
}

#[derive(
    ValueEnum,
    Serialize,
    Deserialize,
    Default,
    Debug,
    Display,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash
)]
/// Use snake_case so we can use the continuous_stable schedule type
#[display(style = "snake_case")]
#[clap(rename_all = "snake_case")]
pub enum ScheduleType {
    #[default]
    #[serde(rename = "diff")]
    Diff,
    #[serde(rename = "continuous")]
    Continuous,
    #[serde(rename = "continuous_stable")]
    ContinuousStable,
    #[serde(rename = "landcastle")]
    Landcastle,
    #[serde(rename = "postcommit")]
    Postcommit,
    #[serde(rename = "testwarden")]
    Testwarden,
    #[serde(rename = "greenwarden")]
    Greenwarden,
    #[serde(rename = "disabled")]
    Disabled,
}

impl ScheduleType {
    /// Mobile build TDs use schedule_type to decide whether we need to run build for changeset (e.g. diff and landcastle)
    /// See UTD implementation: <https://fburl.com/code/wfps6pag>
    pub fn is_changeset_schedule_type(&self) -> bool {
        CHANGESET_SCHEDULE_TYPES.contains(self.to_string().as_str())
    }
}

#[derive(
    ValueEnum,
    Serialize,
    Deserialize,
    Default,
    Debug,
    Display,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash
)]
#[display(style = "snake_case")]
#[clap(rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ContinuousRunMode {
    #[serde(rename = "aarch64")]
    Aarch64,
    #[default]
    Dev,
    Opt,
    OptHourly,
    OptEarlyAdoptor,
    OptAdhoc,
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_schedule_serialization() {
        let s = serde_json::to_string(&ScheduleType::Landcastle);
        assert_eq!(s.unwrap().as_str(), "\"landcastle\"");

        let s = serde_json::to_string(&ScheduleType::ContinuousStable);
        assert_eq!(s.unwrap().as_str(), "\"continuous_stable\"");
    }

    #[test]
    fn test_schedule_deserialization() {
        let s = serde_json::from_str::<ScheduleType>("\"continuous_stable\"");
        assert_eq!(s.unwrap(), ScheduleType::ContinuousStable);

        let s = serde_json::from_str::<ScheduleType>("\"postcommit\"");
        assert_eq!(s.unwrap(), ScheduleType::Postcommit);
    }
}
