/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::process::ExitCode;
use std::process::Termination;

// Exit codes are handled by the [orchestrator classifiers](https://fburl.com/code/k33ivq2j)
const EXIT_CODE_WARNING: u8 = 100;
const EXIT_CODE_SKIPPED: u8 = 101;
const EXIT_CODE_USER_FAILURE: u8 = 102;
const EXIT_CODE_INFRA_FAILURE: u8 = 103;

pub enum WorkflowResult {
    Success,
    Warning(String),
    Skipped(String),
    UserFailure(String),
    InfraFailure(String),
}

impl Termination for WorkflowResult {
    fn report(self) -> std::process::ExitCode {
        match self {
            WorkflowResult::Success => ExitCode::SUCCESS,
            WorkflowResult::Warning(message) => report_verbose(EXIT_CODE_WARNING, message),
            WorkflowResult::Skipped(message) => report_verbose(EXIT_CODE_SKIPPED, message),
            WorkflowResult::UserFailure(message) => report_verbose(EXIT_CODE_USER_FAILURE, message),
            WorkflowResult::InfraFailure(message) => {
                report_verbose(EXIT_CODE_INFRA_FAILURE, message)
            }
        }
    }
}

fn report_verbose(exit_code: u8, message: String) -> std::process::ExitCode {
    eprintln!("\n--------------------------------------\n{}", message);
    ExitCode::from(exit_code)
}
