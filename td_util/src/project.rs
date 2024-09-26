/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! The projects where a verifiable has come from.
//! We should seek to minimize (eventually remove) any project differences.

use std::cmp::Eq;
use std::hash::Hash;
use std::path::PathBuf;
use std::process::Command;

use clap::ValueEnum;
use parse_display::Display;

#[derive(ValueEnum, Debug, Display, Clone, Copy, PartialEq, Eq, Hash)]
#[derive(serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[display(style = "lowercase")]
pub enum TdProject {
    Configerator,
    Fbcode,
    Fbandroid,
    Fbobjc,
    Mobile,
    RL,
    Waandroid,
    Wacommon,
    Www,
    Xplat,
}

impl TdProject {
    pub fn is_mobile(&self) -> bool {
        matches!(self, Self::Fbandroid | Self::Fbobjc)
    }
}

pub fn get_repo_root() -> anyhow::Result<PathBuf> {
    let output = Command::new("hg").arg("root").output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).replace('\n', "");
    Ok(PathBuf::from(stdout))
}
