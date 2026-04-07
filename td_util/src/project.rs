/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The projects where a verifiable has come from.
//! We should seek to minimize (eventually remove) any project differences.

use std::cmp::Eq;
#[cfg(unix)]
use std::ffi::OsString;
use std::hash::Hash;
use std::io;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt as _;
use std::path::PathBuf;
use std::process::Command;

use clap::ValueEnum;
use parse_display::Display;
use serde::Deserialize;
use serde::Serialize;

#[derive(
    ValueEnum,
    Debug,
    Display,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    Default
)]
#[serde(rename_all = "lowercase")]
#[display(style = "lowercase")]
pub enum TdProject {
    Configerator,
    #[default]
    Fbcode,
    Fbandroid,
    Fbobjc,
    Genai,
    Mobile,
    RL,
    Wacommon,
    Waclient,
    Waserver,
    Www,
    Xplat,
    Fasttrack,
}

impl TdProject {
    pub fn is_mobile(&self) -> bool {
        matches!(self, Self::Fbandroid | Self::Fbobjc | Self::Mobile)
    }

    pub fn is_fbsource(&self) -> bool {
        !matches!(self, Self::Configerator | Self::Www)
    }
}

pub fn get_repo_root() -> io::Result<PathBuf> {
    let mut output = Command::new("hg").arg("root").output()?;
    output.stdout.truncate(output.stdout.trim_ascii_end().len());

    #[cfg(unix)]
    let s = OsString::from_vec(output.stdout);
    #[cfg(windows)]
    let s = String::from_utf8(output.stdout).map_err(io::Error::other)?;

    Ok(PathBuf::from(s))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::fbcode(TdProject::Fbcode, true)]
    #[case::fbandroid(TdProject::Fbandroid, true)]
    #[case::fbobjc(TdProject::Fbobjc, true)]
    #[case::genai(TdProject::Genai, true)]
    #[case::mobile(TdProject::Mobile, true)]
    #[case::rl(TdProject::RL, true)]
    #[case::wacommon(TdProject::Wacommon, true)]
    #[case::waclient(TdProject::Waclient, true)]
    #[case::waserver(TdProject::Waserver, true)]
    #[case::xplat(TdProject::Xplat, true)]
    #[case::fasttrack(TdProject::Fasttrack, true)]
    #[case::configerator(TdProject::Configerator, false)]
    #[case::www(TdProject::Www, false)]
    fn test_is_fbsource(#[case] project: TdProject, #[case] expected: bool) {
        assert_eq!(project.is_fbsource(), expected);
    }
}
