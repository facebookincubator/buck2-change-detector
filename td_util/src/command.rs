/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::process::Command;
use std::time::Instant;

use tracing::debug;

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
