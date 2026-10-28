/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

//! The process-wide rayon thread pool.

use anyhow::Context as _;

/// Stack size of each worker thread.
///
/// A worker that waits inside a join runs other queued jobs on its own stack,
/// so the stack depth of nested parallel loops depends on scheduling, not only
/// on the code. Loops over production-size target graphs have overflowed the
/// default 2 MiB stack. 16 MiB is an empirical margin, not a bound.
const WORKER_STACK_BYTES: usize = 16 << 20;

/// Builds the global rayon pool with `WORKER_STACK_BYTES` stacks. Every other
/// setting keeps the rayon default, so `RAYON_NUM_THREADS` still sets the
/// thread count. Call it when the process starts, before any use of the global
/// pool: the pool can be built only once, and its first use, such as a
/// parallel call or `rayon::current_num_threads`, builds it with default
/// stacks.
pub fn init_global() -> anyhow::Result<()> {
    builder()
        .build_global()
        .context("building the global rayon thread pool")
}

/// Shared with the test, which cannot build the global pool: a process can
/// build it only once.
fn builder() -> rayon::ThreadPoolBuilder {
    rayon::ThreadPoolBuilder::new().stack_size(WORKER_STACK_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stack that every worker must have left when it starts a job. A thread
    /// with the default 2 MiB stack has less. The value is fixed, so lowering
    /// `WORKER_STACK_BYTES` to 8 MiB or less fails the test.
    const REQUIRED_STACK_BYTES: usize = 8 << 20;

    #[test]
    fn every_worker_has_a_deep_stack() {
        // Every worker gets the same stack size, so two show it.
        let pool = builder().num_threads(2).build().expect("building a pool");
        let remaining = pool.broadcast(|_| {
            stacker::remaining_stack().expect("the platform reports the stack bounds")
        });
        assert!(
            remaining.iter().all(|&bytes| bytes >= REQUIRED_STACK_BYTES),
            "bytes of stack left on each worker: {remaining:?}"
        );
    }
}
