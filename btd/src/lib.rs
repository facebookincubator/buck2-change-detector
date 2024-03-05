/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

// We use a separate lib since doctests in a binary are ignored,
// and we'd like to use doctests.

#![feature(exit_status_error)]
#![forbid(unsafe_code)]
// Things we disagree with
#![allow(clippy::len_without_is_empty)]

pub mod buck;
pub mod changes;
pub mod check;
pub mod diff;
pub mod glean;
pub mod graph_size;
pub mod output;
pub mod rerun;
pub mod sapling;
pub mod sudo;

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::stdout;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Context as _;
use buck::types::Package;
use clap::Parser;
use itertools::Either;
use itertools::Itertools;
use serde::Serialize;
use td_util::json;
use tempfile::NamedTempFile;
use thiserror::Error;
use tracing::error;
use tracing::info;

use crate::buck::cells::CellInfo;
use crate::buck::run::Buck2;
use crate::buck::targets::BuckTarget;
use crate::buck::targets::Targets;
use crate::buck::types::TargetLabelKeyRef;
use crate::buck::types::TargetPattern;
use crate::changes::Changes;
use crate::check::ValidationError;
use crate::diff::ImpactReason;
use crate::graph_size::GraphSize;
use crate::output::Output;
use crate::output::OutputFormat;
use crate::rerun::PackageStatus;
use crate::sapling::status::read_status;

/// Buck-based target determinator.
#[derive(Parser)]
pub struct Args {
    /// File containing the output of `buck2 audit cell` in the root of the repo.
    /// Otherwise will run the Buck command to figure it out.
    #[arg(long, value_name = "FILE")]
    cells: Option<PathBuf>,

    /// File containing the output of `hg status` for the relevant diff.
    #[arg(long, value_name = "FILE")]
    changes: PathBuf,

    /// File containing the JSON output from `buck2 targets` base the change.
    #[arg(long, value_name = "FILE")]
    base: PathBuf,

    /// File containing the JSON output from `buck2 targets` diff the change.
    /// If left missing, will call `buck2 targets` on the appropriate subset.
    #[arg(long, value_name = "FILE")]
    diff: Option<PathBuf>,

    /// Patterns that represent which targets are of interest, e.g. `fbcode//...`.
    #[arg(long, value_name = "TARGET_PATTERN")]
    universe: Vec<String>,

    // Like `universe`, but without a flag - eventually we'll probably delete --universe.
    /// Patterns that represent which targets are of interest, e.g. `fbcode//...`.
    #[arg(value_name = "TARGET_PATTERN")]
    universe2: Vec<String>,

    /// Number of levels of dependency to explore (default to no limit)
    #[arg(long, value_name = "INT")]
    depth: Option<usize>,

    /// Print out the information in JSON format
    #[arg(long)]
    json: bool,

    /// Print out the information in JSON lines format
    #[arg(long, conflicts_with = "json")]
    json_lines: bool,

    /// Look for prelude rule changes and dirty inputs in response.
    #[arg(long)]
    track_prelude_rule_changes: bool,

    /// The command for running Buck
    #[arg(long, default_value = "buck2")]
    buck: String,

    /// Extra arguments to be passed to Buck
    #[arg(long)]
    buck_arg: Vec<String>,

    /// Isolation directory to use for Buck invocations.
    #[arg(long)]
    isolation_dir: Option<String>,

    /// Arguments passed on to Buck (as `--flagfile`)
    #[arg(long)]
    flagfile: Vec<String>,

    /// Check for dangling edges introduced in the graph.
    #[arg(long)]
    check_dangling: bool,

    /// Glean-specific approach to chasing dependencies.
    #[arg(long)]
    glean: bool,

    /// Show graph size information.
    #[arg(long)]
    graph_size: bool,

    /// Reports all graph errors on the diff revision.
    #[arg(long)]
    write_errors_to_file: Option<PathBuf>,

    /// If a target depends on a target with the label `uses_sudo`, should we propagate the label.
    #[arg(long)]
    propagate_uses_sudo: bool,
}

/// Rather than waiting to deallocate all our big JSON objects, we just forget them with `ManuallyDrop`.
/// This change saves about 10s avoiding deallocating memory at the end.
fn leak_targets(targets: Targets) -> impl Deref<Target = Targets> {
    ManuallyDrop::new(targets)
}

pub fn main(args: Args) -> anyhow::Result<()> {
    let output_format = OutputFormat::from_args(&args);
    let mut buck2 = Buck2::new(args.buck.clone(), args.isolation_dir);

    // All the arguments we should pass on to Buck, when we call it using sensible arguments
    let buck_args = args
        .flagfile
        .iter()
        .flat_map(|x| ["--flagfile".to_owned(), x.to_owned()])
        .chain(args.buck_arg)
        .collect::<Vec<_>>();

    let t = Instant::now();
    let step = |name| info!("Starting {} at {:.3}s", name, t.elapsed().as_secs_f64());

    step("reading cells");
    let cells = match &args.cells {
        Some(file) => CellInfo::new(file)?,
        None => CellInfo::parse(&buck2.cells()?)?,
    };
    step("reading changes");
    let changes = Changes::new(&cells, read_status(&args.changes)?)?;
    step("reading base");
    let base = leak_targets(Targets::from_file(&args.base)?);

    step("validating universe");
    let universe = validate_universe(args.universe.into_iter().chain(args.universe2))?;

    let diff = leak_targets(match &args.diff {
        None => {
            step("computing rerun");
            let (modified_patterns, deleted_packages, merge) =
                compute_rerun(&base, &changes, &mut buck2, &cells, &universe)?;
            if modified_patterns.is_empty() && deleted_packages.is_empty() {
                // No need to ask Buck if we detected nothing changed
                (*base).clone()
            } else {
                let new = if modified_patterns.is_empty() {
                    Targets::new(Vec::new())
                } else {
                    step("running targets");
                    let file = NamedTempFile::new()?;
                    buck2
                        .targets(&buck_args, &modified_patterns, file.path())
                        .with_context(|| format!("When running `{}`", args.buck))?;
                    step("reading diff");
                    Targets::from_file(file.path())?
                };
                if merge {
                    step("merging diff");
                    base.update(new, &deleted_packages)
                } else {
                    new
                }
            }
        }
        Some(diff) => {
            step("reading diff");
            Targets::from_file(diff)?
        }
    });
    step("immediate changes");
    let immediate =
        diff::immediate_target_changes(&base, &diff, &changes, args.track_prelude_rule_changes);

    // Perform inline error validation when we're not collecting errors
    // for downstream reporting.
    if args.write_errors_to_file.is_none() {
        let immediate_targets_only = immediate.iter().collect::<Vec<_>>();
        step("error validation");
        check_empty(&check::check_errors(&base, &diff, &changes))?;
        if args.check_dangling {
            step("dangling check");
            check_empty(&check::check_dangling(
                &base,
                &diff,
                &immediate_targets_only,
                &universe,
            ))
            .context("Dangling target check failed")?;
        }
    }
    let recursive = if args.glean {
        step("glean changes");
        glean::glean_changes(&base, &diff, &changes, args.depth)
    } else {
        step("recursive changes");
        diff::recursive_target_changes(&diff, &immediate, args.depth, |_| true)
    };
    let sudos = if args.propagate_uses_sudo {
        step("recursive sudo labels");
        sudo::requires_sudo_recursively(&diff)
    } else {
        HashSet::new()
    };
    step("printing changes");
    if args.graph_size {
        let mut graph = GraphSize::new(&base, &diff);
        graph.print_recursive_changes(&recursive, &sudos, output_format);
    } else {
        print_recursive_changes(&recursive, &sudos, output_format, |_, x| x);
    }
    // We aggregate errors for post-commit validation so downstream systems
    // can log existing issues.
    if let Some(error_file) = args.write_errors_to_file {
        step("writing all errors to file");
        assert!(!universe.is_empty());
        let errors = check::dump_all_errors(&diff, &universe);

        write_errors_to_file(&errors, error_file, output_format)?;
    }
    let immediate_changes = immediate.len();
    let total_changes = recursive.iter().map(|x| x.len()).sum::<usize>();
    step(&format!(
        "finish with {immediate_changes} immediate changes, {total_changes} total changes"
    ));
    // BTreeMap so that reasons are consistently ordered in logs
    let mut reason_counts: BTreeMap<ImpactReason, u64> = BTreeMap::new();
    for &(_, reason) in recursive.iter().flatten() {
        *reason_counts.entry(reason).or_default() += 1;
    }
    td_util::scuba!(
        event: BTD_SUCCESS,
        duration_ms: t.elapsed().as_millis(),
        data: json!({
            "immediate_changes": immediate_changes,
            "total_changes": total_changes,
            "reason_counts": reason_counts,
        })
    );
    Ok(())
}

/// Tells us which things might have changed, and therefore what
/// we should run buck2 targets on at the diff revision to
/// properly check if it really did change.
fn compute_rerun(
    base: &Targets,
    changes: &Changes,
    buck2: &mut Buck2,
    cells: &CellInfo,
    universe: &[TargetPattern],
) -> anyhow::Result<(Vec<TargetPattern>, HashSet<Package>, bool)> {
    if universe.is_empty() {
        return Err(UniverseError::NoUniverseOrDiff.into());
    }
    let rerun = rerun::rerun(base, changes);
    let mut deleted_packages: HashSet<Package> = HashSet::new();
    let modified_patterns = match &rerun {
        None => universe.to_vec(),
        Some(xs) => {
            // rerun can return packages outside the universe
            // based on what BUCK files are modified. e.g. changes to
            // outside/package/BUCK will rerun foo//outside/package
            let (mut present, unknown): (Vec<_>, Vec<_>) = xs
                .iter()
                .filter(|(x, _)| universe.iter().any(|p| p.matches_package(x)))
                .partition_map(|(x, y)| match y {
                    PackageStatus::Present => Either::Left(x.as_pattern()),
                    PackageStatus::Unknown => Either::Right(x.clone()),
                });
            for x in unknown {
                if buck2.does_package_exist(cells, &x)? {
                    present.push(x.as_pattern());
                } else {
                    deleted_packages.insert(x);
                }
            }
            present
        }
    };
    Ok((modified_patterns, deleted_packages, rerun.is_some()))
}

fn validate_universe(
    universe_arg: impl Iterator<Item = String>,
) -> anyhow::Result<Vec<TargetPattern>> {
    let mut universe = Vec::with_capacity(universe_arg.size_hint().0);
    for u in universe_arg {
        // `buck2 targets` will infer a default cell, but we also use these
        // patterns for filtering where we can't infer the default cell.
        if u.starts_with("//") {
            return Err(UniverseError::MissingQualifier(u).into());
        }
        let pattern = TargetPattern::new(&u);
        // Specific patterns complicate filtering when we use `rerun` to
        // determine what packages were affected by the changeset.
        if pattern.is_specific_target() {
            return Err(UniverseError::ExplicitTarget(u).into());
        }
        universe.push(pattern);
    }
    Ok(universe)
}

#[derive(Debug, Error)]
enum UniverseError {
    #[error(
        "Universe should not use explicit targets, only patterns like `foo//bar/...` and `foo//bar:`. Got `{0}`"
    )]
    ExplicitTarget(String),
    #[error(
        "Universe patterns must have a cell qualifier like `foo//...`, but started with `//`. Got `{0}`"
    )]
    MissingQualifier(String),
    #[error("No universe arguments or `--diff` argument, so don't know what to diff against")]
    NoUniverseOrDiff,
}

#[derive(Debug, Error)]
enum Check {
    #[error("Introduced {0} new errors")]
    NewErrors(usize),
}

fn check_empty(errors: &[ValidationError]) -> anyhow::Result<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        for x in errors {
            error!("{}", x)
        }
        Err(Check::NewErrors(errors.len()).into())
    }
}

impl OutputFormat {
    fn from_args(args: &Args) -> Self {
        if args.json {
            Self::Json
        } else if args.json_lines {
            Self::JsonLines
        } else {
            Self::Text
        }
    }
}

fn print_recursive_changes<'a, T: Serialize + 'a>(
    changes: &[Vec<(&'a BuckTarget, ImpactReason)>],
    sudos: &HashSet<TargetLabelKeyRef>,
    output: OutputFormat,
    mut augment: impl FnMut(&'a BuckTarget, Output<'a>) -> T,
) {
    if output == OutputFormat::Text {
        for (depth, xs) in changes.iter().enumerate() {
            println!("Level {}", depth);
            for (x, _) in xs {
                println!("  {}", x.label());
            }
        }
    } else {
        let items = changes
            .iter()
            .enumerate()
            .flat_map(|(depth, xs)| {
                xs.iter()
                    .map(move |&(x, r)| (depth, x, sudos.contains(&x.label_key()), r))
            })
            .map(|(depth, x, uses_sudo, reason)| {
                augment(x, Output::from_target(x, depth as u64, uses_sudo, reason))
            });

        let out = stdout().lock();
        if output == OutputFormat::Json {
            json::write_json_per_line(out, items).unwrap();
        } else {
            json::write_json_lines(out, items).unwrap();
        }
    }
}

fn write_errors_to_file(
    errors: &[ValidationError],
    error_file: PathBuf,
    output_format: OutputFormat,
) -> anyhow::Result<()> {
    let out = File::create(error_file)?;
    match output_format {
        OutputFormat::Json => {
            json::write_json_per_line(out, errors)?;
        }
        OutputFormat::JsonLines => {
            json::write_json_lines(out, errors)?;
        }
        OutputFormat::Text => {
            // check_empty prints errors if any. We print a summary here.
            if let Err(e) = check_empty(errors) {
                error!("{}", e);
            }
        }
    }
    Ok(())
}
