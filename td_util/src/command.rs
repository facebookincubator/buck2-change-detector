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
use std::ffi::OsString;
use std::io::Write;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;

use anyhow::anyhow;
use itertools::Itertools;
use tempfile::NamedTempFile;
use tracing::debug;

use crate::workflow_error::WorkflowError;

/// Run a command printing out debugging information.
pub fn with_command<T>(
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
/// or any argument escaping.
pub fn display_command(command: &Command) -> String {
    let mut res = command.get_program().to_owned();
    for x in command.get_args() {
        res.push(" ");
        res.push(x);
    }
    res.to_string_lossy().into_owned()
}

/// Spawns a command with stdout piped and returns the child process and stdout handle.
pub fn spawn(mut command: Command) -> Result<(std::process::Child, ChildStdout), WorkflowError> {
    command.stdout(Stdio::piped());
    let mut child = command.spawn().map_err(|err| anyhow!(err))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture stdout"))?;
    Ok((child, stdout))
}

/// Creates a temporary file with the given content and returns an OsString
/// in "@file" format suitable for command line tools that accept file arguments.
///
/// This is commonly used with tools like Buck2 that accept target lists via @file syntax.
pub fn create_at_file_arg<T: AsRef<str>>(
    items: &[T],
    separator: &str,
) -> anyhow::Result<(NamedTempFile, OsString)> {
    let mut file = NamedTempFile::new()?;
    let content = items.iter().map(|x| x.as_ref()).join(separator);
    file.write_all(content.as_bytes())?;
    file.flush()?;

    let mut at_file = OsString::new();
    at_file.push("@");
    at_file.push(file.path());

    Ok((file, at_file))
}

/// The Target Determinator component issuing a Buck command.
///
/// Every Buck invocation declares one, so Buck's own command logging attributes
/// the invocation to the component (as `--client-metadata=id=<client>`) instead
/// of an anonymous caller.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, parse_display::Display)]
#[display(style = "snake_case")]
pub enum BuckClient {
    /// `supertd audit`.
    SupertdAudit,
    /// `supertd btd`.
    SupertdBtd,
    /// `supertd execute`.
    SupertdExecute,
    /// `supertd graph-compressor` and `supertd graph build`.
    SupertdGraphBuild,
    /// `supertd graph-fetch`.
    SupertdGraphFetch,
    /// `supertd graph prepare`.
    SupertdGraphPrepare,
    /// `supertd btd-v2` and `supertd graph select`.
    SupertdGraphSelect,
    /// `supertd llm-select`.
    SupertdLlmSelect,
    /// `supertd proof-store`.
    SupertdProofStore,
    /// `supertd ranker`.
    SupertdRanker,
    /// `supertd resolve-scope`.
    SupertdResolveScope,
    /// `supertd scheduler`.
    SupertdScheduler,
    /// `supertd select`.
    SupertdSelect,
    /// `supertd targets`.
    SupertdTargets,
    /// `supertd verifiable-matcher` and `supertd citadel`.
    SupertdVerifiableMatcher,
    /// The `citrace` binary.
    Citrace,
    /// The `citrace_v2` binary.
    CitraceV2,
    /// The `eval_embedding_tool` binary.
    EvalEmbeddingTool,
    /// The `eval_runner` binary.
    EvalRunner,
}

impl BuckClient {
    /// The `--client-metadata` argument identifying this client. Buck accepts
    /// it before or after the subcommand.
    pub fn metadata_arg(self) -> String {
        format!("--client-metadata=id={self}")
    }

    /// A Buck command for `program` identified as this client, with no subcommand yet.
    pub fn command(self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        command.arg(self.metadata_arg());
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_metadata_ids_are_snake_case() {
        assert_eq!(BuckClient::SupertdBtd.to_string(), "supertd_btd");
        assert_eq!(BuckClient::CitraceV2.to_string(), "citrace_v2");
        assert_eq!(
            BuckClient::SupertdVerifiableMatcher.metadata_arg(),
            "--client-metadata=id=supertd_verifiable_matcher"
        );
    }

    #[test]
    fn client_command_identifies_before_subcommand() {
        let mut command = BuckClient::SupertdTargets.command("buck2");
        command.arg("targets");
        assert_eq!(
            display_command(&command),
            "buck2 --client-metadata=id=supertd_targets targets"
        );
    }
}
