/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

#![forbid(unsafe_code)]

use td_util::cli::parse_args;
use td_util::workflow_error::WorkflowError;

#[fbinit::main]
pub fn main(fb: fbinit::FacebookInit) -> Result<(), WorkflowError> {
    let _guard = td_util::init(fb);
    audit::main(parse_args()?)
}
