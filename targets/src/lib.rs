/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

// We use a separate lib since doctests in a binary are ignored,
// and we'd like to use doctests.
#![feature(iter_intersperse)]

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process;
use std::process::Command;

use clap::Parser;

/// Run `buck2 targets` with all the arguments required for BTD/Citadel.
#[derive(Parser)]
pub struct Args {
    /// The command for running Buck
    #[arg(long, default_value = "buck2")]
    buck: String,

    /// Where to write the output - otherwise gets written to stdout.
    /// Equivalent to passing `-- --output=FILE` as additional arguments.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,

    #[arg(long)]
    dry_run: bool,

    /// Arguments passed onwards - typically patterns.
    #[arg(value_name = "ARGS")]
    arguments: Vec<String>,
}

pub fn targets_arguments() -> &'static [&'static str] {
    &[
        "targets",
        "--streaming",
        "--keep-going",
        "--no-cache",
        "--show-unconfigured-target-hash",
        "--json-lines",
        "--output-attribute=^buck\\.|^name$|^labels$|^ci_srcs$|^ci_deps$",
        "--imports",
        // `buck.cfg_modifiers` is PACKAGE value key for modifiers which may change configurations of all targets
        // covered by the PACKAGE. We need BTD to specifically query for these PACKAGE values because buck currently
        // does not hash PACKAGE modifiers for target hashing.
        // TODO(scottcao): Remove `buck.cfg_modifiers` once we have a way to hash PACKAGE modifiers.
        "--package-values-regex=^citadel\\.labels$|^buck\\.cfg_modifiers$",
    ]
}

pub fn main(args: Args) -> anyhow::Result<()> {
    run(&args.buck, args.output, args.dry_run, &args.arguments)
}

/// This function runs the `buck2 targets` command, utilizing various arguments to optimize its behavior for BTD/Citadel.
/// The output can either be written to stdout or to a specified output file.
///
/// ### Arguments
///
/// * `buck` - The command to run Buck, typically "buck2".
/// * `output_file` - Optional path to the file where the output will be written. If not provided, the output is written to stdout.
/// * `dry_run` - If set to `true`, the command will print the command that would have been executed instead of executing it, without executing it.
/// * `arguments` - Additional arguments typically provided as patterns to be passed to the `buck2 targets` command.
pub fn run(
    buck: &str,
    output_file: Option<PathBuf>,
    dry_run: bool,
    arguments: &[String],
) -> anyhow::Result<()> {
    let t = std::time::Instant::now();
    let mut command = Command::new(buck);
    command.args(targets_arguments());
    if let Some(x) = &output_file {
        command.arg("--output");
        command.arg(x);
    }
    command.args(arguments);

    if dry_run {
        println!(
            "{} {}",
            buck,
            command
                .get_args()
                .intersperse(OsStr::new(" "))
                .collect::<OsString>()
                .to_string_lossy()
        );
        return Ok(());
    }

    let status = command.status()?;
    if status.success() {
        td_util::scuba!(
            event: TARGETS_SUCCESS,
            duration_ms: t.elapsed().as_millis() as u64,
        );
        Ok(())
    } else {
        process::exit(status.code().unwrap_or(1));
    }
}
