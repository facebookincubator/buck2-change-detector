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
use std::hash::Hash;
use std::str::FromStr;

use clap::ValueEnum;
use parse_display::Display;
use serde::Deserialize;
use serde::Serialize;

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
/// Represents the different phases of validation where we run CI.
pub enum ScheduleType {
    #[default]
    Diff,
    Continuous,
    ContinuousStable,
    Landcastle,
    Postcommit,
    Testwarden,
    Greenwarden,
    Disabled,
    Master,
    Relbranch,
    Coverage,
}

impl ScheduleType {
    /// Mobile build TDs use schedule_type to decide whether we need to run build for changeset (e.g. diff and landcastle)
    /// See UTD implementation: <https://fburl.com/code/wfps6pag>
    pub fn is_changeset_schedule_type(&self) -> bool {
        match self {
            ScheduleType::Diff
            | ScheduleType::Landcastle
            | ScheduleType::Master
            | ScheduleType::Postcommit
            | ScheduleType::Relbranch => true,
            _ => false,
        }
    }

    pub fn is_trunk_schedule_type(&self) -> bool {
        match self {
            ScheduleType::Continuous
            | ScheduleType::ContinuousStable
            | ScheduleType::Testwarden
            | ScheduleType::Greenwarden
            | ScheduleType::Disabled => true,
            _ => false,
        }
    }

    /// For checking a ScheduleType that an orchestrator is running with against a ScheduleType target is configured with
    pub fn accepts(self, other: &ScheduleType) -> bool {
        match self {
            ScheduleType::Continuous => {
                matches!(other, ScheduleType::Continuous | ScheduleType::Diff)
            }
            ScheduleType::Testwarden => {
                matches!(
                    other,
                    ScheduleType::Testwarden | ScheduleType::Continuous | ScheduleType::Diff
                )
            }
            ScheduleType::ContinuousStable => {
                matches!(
                    other,
                    ScheduleType::ContinuousStable | ScheduleType::Continuous | ScheduleType::Diff
                )
            }
            _ => *other == self,
        }
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
/// Trunk runs with a specific purpose.
pub enum ContinuousRunMode {
    #[serde(rename = "aarch64")]
    Aarch64,
    #[default]
    AsicHourly,
    Dev,
    Opt,
    OptHourly,
    OptEarlyAdoptor,
    OptAdhoc,
    RunwayShadow,
    RunwayCoverage,
    TranslatorHourly,
    TranslatorNightly,
    TranslatorWeekend,
    // To be used when shceduling Rl Translator jobs for multisect
    TranslatorContinuousForMultisect,
}

impl ContinuousRunMode {
    pub fn to_translator_run_type(&self) -> &'static str {
        match self {
            ContinuousRunMode::TranslatorHourly => "hourly",
            ContinuousRunMode::TranslatorNightly => "mightly",
            ContinuousRunMode::TranslatorWeekend => "weekend",
            _ => "Unknown",
        }
    }
}

impl FromStr for ContinuousRunMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hourly" => Ok(ContinuousRunMode::TranslatorHourly),
            "nightly" => Ok(ContinuousRunMode::TranslatorNightly),
            "weekend" => Ok(ContinuousRunMode::TranslatorWeekend),
            _ => Err(()),
        }
    }
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
