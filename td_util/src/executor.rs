/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! Adapter to bridge the gap between sync and async code.

use std::future::Future;

// Run future within a sync context
// Uses existing Tokio runtime if available, otherwise creates a new one.
pub fn run_as_sync<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            let _guard = handle.enter();
            // `block_on` never yields back to Tokio, so the caller's coop budget is
            // never refilled. Once it runs out, every budgeted poll (e.g. a
            // `JoinHandle` per Thrift stream item) returns `Pending` and re-wakes
            // itself, spinning forever.
            futures::executor::block_on(tokio::task::coop::unconstrained(future))
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

    // Multi-threaded, like the `fbinit::main` runtime, so the spawned tasks can
    // run while `run_as_sync` blocks this thread.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_run_as_sync_with_runtime_outlasts_coop_budget() {
        // Tokio's per-task coop budget is 128 units and each `JoinHandle` poll
        // spends one, so this would spin forever if the budget applied.
        let handles: Vec<_> = (0..1000).map(|i| tokio::spawn(async move { i })).collect();
        let sum = run_as_sync(async {
            let mut sum = 0;
            for handle in handles {
                sum += handle.await.unwrap();
            }
            sum
        });
        assert_eq!(sum, (0..1000).sum::<i32>());
    }
}
