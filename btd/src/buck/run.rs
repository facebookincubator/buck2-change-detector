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
use std::time::Instant;

use anyhow::Context as _;
use itertools::Itertools;
use targets::targets_arguments;
use tempfile::NamedTempFile;
use tracing::debug;

use crate::buck::cells::CellInfo;
use crate::buck::config::cell_build_files;
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
            Err(anyhow::anyhow!(
                "Output of `root` was `{}`, which does not exist",
                path.display()
            ))
        } else {
            Ok(path)
        }
    }

    pub fn cells(&mut self) -> anyhow::Result<String> {
        let mut command = self.command();
        command.args(["audit", "cell", "--json"]);
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
        for build_file in cell_build_files(&x.cell()) {
            if root
                .join(cells.resolve(&x.join_path(build_file))?.as_str())
                .exists()
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

/// Run a command printing out debugging information
fn with_command<T>(
    command: Command,
    run: impl Fn(Command) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    debug!("Running: {}", display_command(&command));
    let start = Instant::now();
    let res = run(command)?;
    debug!("Command succeeded in {:.2}s", start.elapsed().as_secs_f64());
    Ok(res)
}

/// Works only for command lines we produce, without environment variables
/// or any argument escaping
fn display_command(command: &Command) -> String {
    let mut res = command.get_program().to_owned();
    for x in command.get_args() {
        res.push(" ");
        res.push(x);
    }
    res.to_string_lossy().into_owned()
}
