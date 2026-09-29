/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStdout;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::anyhow;
use tempfile::TempDir;
use tracing::info;
use tracing::warn;

use crate::cancellation::Cancellation;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Controls how a nested Buck invocation reports diagnostics.
pub enum BuckDiagnostics {
    /// Preserve Buck's normal console output.
    #[default]
    Inherit,
    /// Suppress routine console output and report a Buck UI link.
    QuietWithUi,
}

struct BuildIdOutput {
    path: PathBuf,
    _directory: TempDir,
}

/// A running Buck child process and its diagnostic output state.
pub struct BuckProcess {
    child: RunningChild,
    build_id: Option<BuildIdOutput>,
}

/// A Buck `targets` command whose diagnostics are placed before caller arguments.
pub struct BuckCommand {
    command: Command,
    build_id: Option<BuildIdOutput>,
    cancellation: Option<Cancellation>,
}

enum RunningChild {
    Direct(Child),
    Cancellable {
        child: Arc<Mutex<Child>>,
        cancellation: Cancellation,
    },
}

impl BuckCommand {
    /// Construct a Buck `targets` command with diagnostics before caller arguments.
    pub fn targets(
        program: impl AsRef<OsStr>,
        isolation_dir: Option<&str>,
        diagnostics: BuckDiagnostics,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new(program);
        if let Some(isolation_dir) = isolation_dir {
            command.args(["--isolation-dir", isolation_dir]);
        }
        command.arg("targets");
        let build_id = match diagnostics {
            BuckDiagnostics::Inherit => None,
            BuckDiagnostics::QuietWithUi => {
                let directory = tempfile::tempdir().context("creating Buck build-ID directory")?;
                let path = directory.path().join("build_id");
                // The "none" console also discards streaming stdout from targets.
                command
                    .args(["--console=simple", "--verbose=0"])
                    .arg(format!("--write-build-id={}", path.display()));
                Some(BuildIdOutput {
                    path,
                    _directory: directory,
                })
            }
        };

        Ok(Self {
            command,
            build_id,
            cancellation: None,
        })
    }

    /// Bind this targets invocation to its preparation's cancellation lifecycle.
    pub fn with_cancellation(mut self, cancellation: Option<Cancellation>) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Access the command to append `targets` options and patterns.
    pub fn command(&mut self) -> &mut Command {
        &mut self.command
    }

    /// Access the final command for logging.
    pub fn as_command(&self) -> &Command {
        &self.command
    }

    /// Spawn the configured command.
    pub fn spawn(mut self) -> anyhow::Result<BuckProcess> {
        let child = match self.cancellation {
            Some(cancellation) => RunningChild::Cancellable {
                child: cancellation.spawn(&mut self.command)?,
                cancellation,
            },
            None => RunningChild::Direct(self.command.spawn().context("spawning Buck")?),
        };
        Ok(BuckProcess {
            child,
            build_id: self.build_id,
        })
    }
}

impl BuckProcess {
    /// Take the child's piped stdout.
    pub fn take_stdout(&mut self) -> anyhow::Result<ChildStdout> {
        match &mut self.child {
            RunningChild::Direct(child) => child.stdout.take(),
            RunningChild::Cancellable { child, .. } => child
                .lock()
                .expect("Buck child mutex poisoned")
                .stdout
                .take(),
        }
        .context("capturing Buck stdout")
    }

    /// Wait for Buck and report its build UI on failure when available.
    pub fn wait(self) -> anyhow::Result<()> {
        self.wait_with_output(Ok(()))
    }

    /// Wait for Buck while preserving a caller's stdout-processing result.
    pub fn wait_with_output<T>(mut self, output: anyhow::Result<T>) -> anyhow::Result<T> {
        let status = match &mut self.child {
            RunningChild::Direct(child) => child.wait().context("waiting for Buck")?,
            RunningChild::Cancellable {
                child,
                cancellation,
            } => cancellation.wait(child)?,
        };
        let diagnostic = match read_build_id(self.build_id.as_ref()) {
            Ok(Some(build_id)) => {
                let url = format!("https://www.internalfb.com/buck2/{build_id}");
                info!("Buck UI: {url}");
                format!("; Buck UI: {url}")
            }
            Ok(None) => String::new(),
            Err(error) => {
                warn!("Failed to read Buck build ID: {error:#}");
                format!("; failed to read Buck build ID: {error:#}")
            }
        };

        match (status.success(), output) {
            (true, Ok(output)) => Ok(output),
            (true, Err(error)) => {
                Err(error.context(format!("Buck command completed with {status}{diagnostic}")))
            }
            (false, Ok(_)) => Err(anyhow!("Buck command failed with {status}{diagnostic}")),
            (false, Err(error)) => Err(error.context(format!(
                "Buck command failed with {status}; stdout processing also failed{diagnostic}"
            ))),
        }
    }
}

impl Drop for RunningChild {
    fn drop(&mut self) {
        let cleanup = |child: &mut Child| -> anyhow::Result<()> {
            if child
                .try_wait()
                .context("checking owned Buck child")?
                .is_none()
            {
                child
                    .kill()
                    .context("stopping unfinished owned Buck child")?;
                child.wait().context("reaping owned Buck child")?;
            }
            Ok(())
        };
        let result = match self {
            Self::Direct(child) => cleanup(child),
            Self::Cancellable { child, .. } => {
                cleanup(&mut child.lock().expect("Buck child mutex poisoned"))
            }
        };
        if let Err(error) = result {
            warn!("Buck child cleanup failed: {error:#}");
        }
    }
}

fn read_build_id(output: Option<&BuildIdOutput>) -> anyhow::Result<Option<String>> {
    let Some(output) = output else {
        return Ok(None);
    };
    let value = match fs::read_to_string(&output.path) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("reading Buck build ID from {}", output.path.display()));
        }
    };
    Ok((!value.trim().is_empty()).then(|| value.trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_command_places_native_flags_before_caller_arguments() {
        let mut command =
            BuckCommand::targets("buck2", Some("isolation"), BuckDiagnostics::QuietWithUi)
                .expect("should construct Buck command");
        command.command().args(["--", "//example:target"]);

        let arguments = command
            .as_command()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments[0..5],
            [
                "--isolation-dir",
                "isolation",
                "targets",
                "--console=simple",
                "--verbose=0",
            ]
        );
        assert!(arguments[5].starts_with("--write-build-id="));
        assert_eq!(arguments[6..], ["--", "//example:target"]);
    }

    #[test]
    fn reads_trimmed_build_id() {
        let command = BuckCommand::targets("buck2", None, BuckDiagnostics::QuietWithUi)
            .expect("should construct Buck command");
        let output = command
            .build_id
            .as_ref()
            .expect("quiet diagnostics should allocate build-ID output");
        fs::write(&output.path, " test-buck-uuid\n").expect("should write build ID");

        assert_eq!(
            read_build_id(Some(output)).expect("should read build ID"),
            Some("test-buck-uuid".to_owned())
        );
    }
}
