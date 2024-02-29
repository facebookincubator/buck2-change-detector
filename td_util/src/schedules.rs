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
    Landcastle, // Should this be just "land"?
}

impl ScheduleType {
    /// Mobile build TDs use schedule_type to decide whether we need to run build for changeset (e.g. diff and landcastle)
    /// See UTD implementation: <https://fburl.com/code/wfps6pag>
    pub fn is_changeset_schedule_type(&self) -> bool {
        CHANGESET_SCHEDULE_TYPES.contains(self.to_string().as_str())
    }
}
