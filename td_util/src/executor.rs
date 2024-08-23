/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Adapter to bridge the gap between sync and async code.

use std::future::Future;

// Run future within a sync context
// Uses existing Tokio runtime if available, otherwise creates a new one.
pub fn run_as_sync<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let _guard = handle.enter();
            futures::executor::block_on(future)
        }
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future),
    }
}

#[cfg(test)]
mod tests {
    use crate::executor::run_as_sync;

    #[test]
    fn test_run_as_sync_without_runtime() {
        // Should not panic and should return the result of the future.
        assert_eq!(run_as_sync(async { 42 }), 42);
    }

    #[fbinit::test]
    async fn test_run_as_sync_with_runtime() {
        // Tokio runtime is injected by fbinit. Still, should not panic and should return the result of the future.
        assert_eq!(run_as_sync(async { 42 }), 42);
    }
}
