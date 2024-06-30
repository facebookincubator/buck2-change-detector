/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context as _;
use audit::audit_cell_arguments;
use audit::audit_config_arguments;
use itertools::Itertools;
use targets::targets_arguments;
use td_util::command::with_command;
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::buck::cells::CellInfo;
use crate::buck::types::Package;
use crate::buck::types::TargetPattern;

/// A struct to represent running Buck2 commands.
/// All methods are `&mut` to avoid simultaneous Buck2 commands.
pub struct Buck2 {
    /// The program to invoke, normally `buck2`.
    program: String,
    /// The result of running `root`, if we have done so yet.
    root: Option<PathBuf>,
    /// The isolation directory to always use when invoking buck
    isolation_dir: Option<String>,
}

#[derive(Error, Debug)]
enum Buck2Error {
    #[error("Output of `root` was `{}`, which does not exist", .0.display())]
    RootDoesNotExist(PathBuf),
}

impl Buck2 {
    pub fn new(program: String, isolation_dir: Option<String>) -> Self {
        Self {
            program,
            root: None,
            isolation_dir,
        }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        match &self.isolation_dir {
            None => {}
            Some(isolation_dir) => {
                command.args(["--isolation-dir", isolation_dir]);
            }
        }
        command
    }

    pub fn root(&mut self) -> anyhow::Result<PathBuf> {
        match &mut self.root {
            None => {
                let res = self.root_uncached()?;
                self.root = Some(res.clone());
                Ok(res)
            }
            Some(x) => Ok(x.clone()),
        }
    }

    fn root_uncached(&mut self) -> anyhow::Result<PathBuf> {
        let mut command = self.command();
        command.args(["root", "--kind=project"]);
        let res = with_command(command, |mut command| {
            let res = command.output()?;
            res.status.exit_ok().with_context(|| {
                format!("Buck2 stderr: {}", String::from_utf8_lossy(&res.stderr))
            })?;
            Ok(res)
        })?;
        let path = PathBuf::from(String::from_utf8(res.stdout)?.trim());
        // Sanity check the output
        if !path.exists() {
            Err(Buck2Error::RootDoesNotExist(path).into())
        } else {
            Ok(path)
        }
    }

    pub fn cells(&mut self) -> anyhow::Result<String> {
        let mut command = self.command();
        command.args(audit_cell_arguments());
        command.current_dir(self.root()?);
        let res = with_command(command, |mut command| {
            let res = command.output()?;
            res.status.exit_ok().with_context(|| {
                format!("Buck2 stderr: {}", String::from_utf8_lossy(&res.stderr))
            })?;
            Ok(res)
        })?;
        Ok(String::from_utf8(res.stdout)?)
    }

    pub fn audit_config(&mut self) -> anyhow::Result<String> {
        let mut command = self.command();
        command.args(audit_config_arguments());
        command.current_dir(self.root()?);
        let res = with_command(command, |mut command| {
            let res = command.output()?;
            res.status.exit_ok().with_context(|| {
                format!("Buck2 stderr: {}", String::from_utf8_lossy(&res.stderr))
            })?;
            Ok(res)
        })?;
        Ok(String::from_utf8(res.stdout)?)
    }

    /// Does a package exist. Doesn't actually invoke Buck2, but does look at the file system.
    pub fn does_package_exist(&mut self, cells: &CellInfo, x: &Package) -> anyhow::Result<bool> {
        let root = self.root()?;
        for build_file in cells.build_files(&x.cell())? {
            let cell_path = x.join_path(build_file);
            if !cells.is_ignored(&cell_path)
                && root.join(cells.resolve(&cell_path)?.as_str()).exists()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn targets(
        &mut self,
        extra_args: &[String],
        targets: &[TargetPattern],
        output: &Path,
    ) -> anyhow::Result<()> {
        assert!(!targets.is_empty());

        let mut file = NamedTempFile::new()?;
        let target_data = targets.iter().map(|x| x.as_str()).join("\n");
        file.write_all(target_data.as_bytes())?;
        file.flush()?;
        let mut at_file = OsString::new();
        at_file.push("@");
        at_file.push(file.path());

        let mut command = self.command();
        command
            .args(targets_arguments())
            .arg("--output")
            .arg(output)
            .arg(at_file)
            .args(extra_args);

        with_command(command, |mut command| Ok(command.status()?.exit_ok()?))
    }
}
