/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::process::Termination;

use clap::CommandFactory;
use clap::FromArgMatches;
use clap::Parser;
use fbinit::FacebookInit;
use td_util::cli::get_args;
use td_util::executor::run_as_sync;

/// Generic binary for the pieces of the new target-determinator framework.
#[allow(clippy::large_enum_variant)] // Only one instance, so not a big deal
#[derive(Parser)]
#[command(name = "supertd", version = get_version())]
enum Args {
    Audit(audit::Args),
    Btd(btd::Args),
    #[cfg(fbcode_build)]
    Citadel(verifiable_matcher::Args),
    #[cfg(fbcode_build)]
    VerifiableMatcher(verifiable_matcher::Args),
    #[cfg(fbcode_build)]
    Ranker(ranker::Args),
    #[cfg(fbcode_build)]
    Rerun(rerun::Args),
    #[cfg(fbcode_build)]
    Scheduler(scheduler::Args),
    Targets(targets::Args),
    #[cfg(all(fbcode_build, target_os = "linux"))]
    Verse(verse_citadel_adaptor::Args),
}

#[fbinit::main]

pub fn main(fb: FacebookInit) -> ExitCode {
    let _guard = td_util::init(fb);

    let mut command = Args::command();
    if std::env::var_os("SUPERTD_IGNORE_EXTRA_ARGUMENTS") == Some("1".into()) {
        // We don't want to turn on ignore_errors unconditionally for a few reasons:
        // 1. It means we won't stop mistakes.
        // 2. It breaks the nested `--help` output.
        // But we might want to have it briefly on for a rollout.
        command = command.ignore_errors(true);
    }
    let args = match get_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{}", err.context("Error parsing arguments"));
            return ExitCode::FAILURE;
        }
    };

    let args = match Args::from_arg_matches(&command.get_matches_from(args)) {
        Err(err) => {
            eprintln!("{}", err.format(&mut Args::command()));
            return ExitCode::FAILURE;
        }
        Ok(args) => args,
    };

    let ret = match args {
        Args::Audit(args) => audit::main(args),
        Args::Btd(args) => btd::main(args),
        #[cfg(fbcode_build)]
        Args::Citadel(args) => run_as_sync(verifiable_matcher::main(args)),
        #[cfg(fbcode_build)]
        Args::VerifiableMatcher(args) => run_as_sync(verifiable_matcher::main(args)),
        #[cfg(fbcode_build)]
        Args::Ranker(args) => run_as_sync(ranker::main(args)),
        #[cfg(fbcode_build)]
        Args::Rerun(args) => run_as_sync(rerun::main(fb, args)),
        #[cfg(fbcode_build)]
        Args::Scheduler(args) => run_as_sync(scheduler::main(fb, args)),
        Args::Targets(args) => targets::main(args),
        #[cfg(all(fbcode_build, target_os = "linux"))]
        Args::Verse(args) => verse_citadel_adaptor::main(args),
    };

    match ret {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => err.report(),
    }
}

#[cfg(fbcode_build)]
fn get_version() -> &'static str {
    cli_version::get_version()
}

#[cfg(not(fbcode_build))]
fn get_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use clap::Command;

    use super::*;

    #[test]
    fn test_args_valid() {
        // Ensure invalid arguments give us errors,
        // work around https://github.com/clap-rs/clap/issues/3133
        fn check(x: &mut Command) {
            x.render_long_help();
            for x in x.get_subcommands_mut() {
                check(x);
            }
        }
        check(&mut Args::command());
    }
}
