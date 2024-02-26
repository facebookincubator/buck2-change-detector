/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Utilities for working with the `tracing` crate.
//! Ensure all supertd projects have a consistent way of logging.

use std::io::stderr;
use std::io::stdout;
use std::io::IsTerminal;

use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Set up tracing so it prints to stderr, and can be used for output.
/// Most things should use `info` and `debug` level for showing messages.
pub fn init_tracing() {
    let mut env_filter = EnvFilter::from_default_env();
    if std::env::var_os("RUST_LOG").is_none() {
        // Enable info log by default, debug log for target determinator packages
        let directives = vec![
            "info",
            "btd=debug",
            "citadel=debug",
            "verifiable_matcher=debug",
            "ranker=debug",
            "scheduler=debug",
            "targets=debug",
            "verse=debug",
        ];
        for directive in directives {
            env_filter =
                env_filter.add_directive(directive.parse().expect("bad hardcoded log directive"));
        }
    }

    let layer = tracing_subscriber::fmt::layer()
        .with_line_number(false)
        .with_file(false)
        .with_writer(stderr)
        .with_ansi(stdout().is_terminal())
        .with_target(false)
        .with_filter(env_filter);

    tracing_subscriber::registry().with(layer).init();
}
