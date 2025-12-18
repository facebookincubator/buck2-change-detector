/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

#![feature(exit_status_error)]
#![forbid(unsafe_code)]

use td_util::supertd_events;
pub mod cells;
pub mod config;
pub mod glob;
pub mod ignore_set;
pub mod labels;
pub mod owners;
pub mod package_resolver;
pub mod run;
pub mod select;
pub mod target_graph;
pub mod target_map;
pub mod targets;
pub mod types;
use td_util::tracing;

pub fn init(fb: fbinit::FacebookInit) -> supertd_events::ScubaClientGuard {
    tracing::init_tracing();
    supertd_events::init(fb)
}
