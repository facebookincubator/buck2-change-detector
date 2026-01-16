/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! CLI command to log BTD graph cache lookup metadata to Scuba.
//!
//! Reads the JSON metadata file produced by BTDCachedGraphDownloaderScriptController.php
//! and logs it as a BTD_GRAPH_CACHE_LOOKUP event.

use std::path::PathBuf;

use clap::Parser;

use crate::workflow_error::WorkflowError;

/// CLI arguments for the log-graph-cache subcommand.
#[derive(Parser, Debug)]
pub struct Args {
    /// Path to the metadata JSON file produced by BTDCachedGraphDownloaderScriptController
    #[arg(long)]
    pub metadata_file: PathBuf,
}

/// Read graph cache lookup metadata from a file and log it to Scuba.
pub fn main(args: Args) -> Result<(), WorkflowError> {
    let start = std::time::Instant::now();

    let contents = std::fs::read_to_string(&args.metadata_file).map_err(|e| {
        anyhow::anyhow!(
            "Failed to read metadata file {:?}: {}",
            args.metadata_file,
            e
        )
    })?;

    let metadata: serde_json::Value = serde_json::from_str(&contents)
        .map_err(|e| anyhow::anyhow!("Failed to parse metadata JSON: {}", e))?;

    crate::scuba!(
        event: BTD_GRAPH_CACHE_LOOKUP,
        duration: start.elapsed(),
        data: metadata
    );

    Ok(())
}
