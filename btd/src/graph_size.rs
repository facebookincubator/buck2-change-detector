/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::stdout;

use rayon::prelude::*;
use serde::Serialize;
use td_util::json;
use td_util::no_hash::BuildNoHash;
use td_util_buck::targets::BuckTarget;
use td_util_buck::targets::Targets;
use td_util_buck::types::TargetLabel;
use td_util_buck::types::TargetLabelKeyRef;

use crate::diff::ImpactTraceData;
use crate::output::Output;
use crate::output::OutputFormat;

pub struct GraphSize {
    base: TargetsSize,
    diff: TargetsSize,
}

struct TargetsSize {
    deps_one: HashMap<TargetLabel, HashSet<TargetLabel>, BuildNoHash>,
}

impl TargetsSize {
    fn new(data: &Targets) -> Self {
        let mut deps_one = HashMap::with_hasher(BuildNoHash::default());
        for x in data.targets() {
            deps_one.insert(x.label(), x.deps.iter().cloned().collect());
        }

        Self { deps_one }
    }

    fn dfs(&self, label: &TargetLabel, visited: &mut HashSet<TargetLabel, BuildNoHash>) {
        // This code could be written more simply as `visited.insert(label)` but that
        // slows things down by about 10%. I don't understand why.
        // See D51173107 for benchmarks.
        if !visited.contains(label) {
            visited.insert(label.clone());
            for x in self.deps_one.get(label).into_iter().flatten() {
                self.dfs(x, visited)
            }
        }
    }

    fn get(&self, label: &TargetLabel) -> usize {
        let mut visited = HashSet::with_hasher(BuildNoHash::default());
        self.dfs(label, &mut visited);
        visited.len()
    }
}

#[derive(Serialize)]
struct OutputWithSize<'a> {
    #[serde(flatten)]
    output: Output<'a>,
    before_size: usize,
    after_size: usize,
}

impl GraphSize {
    pub fn new(base: &Targets, diff: &Targets) -> Self {
        Self {
            base: TargetsSize::new(base),
            diff: TargetsSize::new(diff),
        }
    }

    pub fn print_recursive_changes(
        &mut self,
        changes: &[Vec<(&BuckTarget, ImpactTraceData)>],
        sudos: &HashSet<TargetLabelKeyRef>,
        output: OutputFormat,
    ) {
        let items = changes
            .iter()
            .enumerate()
            .flat_map(|(depth, xs)| {
                xs.iter()
                    .map(move |&(x, ref r)| (depth, x, sudos.contains(&x.label_key()), r.clone()))
            })
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(depth, x, uses_sudo, reason)| OutputWithSize {
                output: Output::from_target(x, depth as u64, uses_sudo, reason),
                before_size: self.base.get(&x.label()),
                after_size: self.diff.get(&x.label()),
            })
            .collect::<Vec<_>>();

        let out = stdout().lock();
        if output == OutputFormat::Json {
            json::write_json_per_line(out, items).unwrap();
        } else {
            json::write_json_lines(out, items).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use td_util_buck::targets::TargetsEntry;

    use super::*;

    fn mk_label(x: &str) -> TargetLabel {
        TargetLabel::new(&format!("none//:{x}"))
    }

    #[test]
    fn test_graph_size() {
        let graph = [
            ("a", vec!["b", "c"]),
            ("b", vec!["d"]),
            ("c", vec!["d", "e"]),
            ("d", vec!["f"]),
            ("f", vec!["g"]),
        ];

        let targets = Targets::new(
            graph
                .iter()
                .map(|(name, deps)| {
                    TargetsEntry::Target(BuckTarget {
                        deps: deps.iter().map(|x| mk_label(x)).collect(),
                        ..BuckTarget::testing(name, "none//", "rule_type")
                    })
                })
                .collect(),
        );
        let targets_size = TargetsSize::new(&targets);

        assert_eq!(targets_size.get(&mk_label("g")), 1);
        assert_eq!(targets_size.get(&mk_label("f")), 2);
        assert_eq!(targets_size.get(&mk_label("e")), 1);
        assert_eq!(targets_size.get(&mk_label("d")), 3);
        assert_eq!(targets_size.get(&mk_label("c")), 5);
        assert_eq!(targets_size.get(&mk_label("b")), 4);
        assert_eq!(targets_size.get(&mk_label("a")), 7);
    }

    #[test]
    fn test_graph_size_cycle() {
        let graph = [("a", vec!["b", "c"]), ("b", vec!["a"])];

        let targets = Targets::new(
            graph
                .iter()
                .map(|(name, deps)| {
                    TargetsEntry::Target(BuckTarget {
                        deps: deps.iter().map(|x| mk_label(x)).collect(),
                        ..BuckTarget::testing(name, "none//", "rule_type")
                    })
                })
                .collect(),
        );
        let targets_size = TargetsSize::new(&targets);

        assert_eq!(targets_size.get(&mk_label("a")), 3);
        assert_eq!(targets_size.get(&mk_label("b")), 3); // b -> a -> c
        assert_eq!(targets_size.get(&mk_label("c")), 1);
    }
}
