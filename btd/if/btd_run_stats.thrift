/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 *
 * @oncall: ci_efficiency
 */

include "thrift/annotation/rust.thrift"

@rust.Exhaustive
struct BTDRunStats {
  1: i64 base_graph_size;
  2: i64 target_graph_size;
}
