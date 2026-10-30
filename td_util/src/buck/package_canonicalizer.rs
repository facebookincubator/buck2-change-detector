/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is dual-licensed under either the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree or the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree. You may select, at your option, one of the
 * above-listed licenses.
 */

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use dashmap::DashMap;
use serde::Serialize;
use tracing::info;
use tracing::warn;

use crate::cells::CellInfo;
use crate::types::CellPath;
use crate::types::ProjectRelativePath;

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct CanonicalizationStats {
    pub labels_rewritten: usize,
    pub packages_resolved_through_symlink: usize,
    pub resolver_errors: usize,
}

const RESOLVER_ERROR_SAMPLE_LIMIT: usize = 10;

#[derive(Default)]
struct ResolverDiagnostics {
    kinds: BTreeMap<String, usize>,
    samples: Vec<(Option<ErrorKind>, String)>,
}

/// Resolves directory aliases once per package, without changing ordinary cell spellings.
pub struct PackageCanonicalizer<'a> {
    cells: &'a CellInfo,
    root: Option<PathBuf>,
    packages: DashMap<String, Option<String>>,
    labels_rewritten: AtomicUsize,
    packages_resolved_through_symlink: AtomicUsize,
    resolver_errors: AtomicUsize,
    diagnostics: Mutex<ResolverDiagnostics>,
}

impl<'a> PackageCanonicalizer<'a> {
    pub fn new(cells: &'a CellInfo) -> anyhow::Result<Self> {
        let (root, error) = match fs::canonicalize(cells.root()) {
            Ok(root) => (Some(root), None),
            Err(error) => (None, Some(anyhow::Error::from(error))),
        };
        let canonicalizer = Self {
            cells,
            root,
            packages: DashMap::new(),
            labels_rewritten: AtomicUsize::new(0),
            packages_resolved_through_symlink: AtomicUsize::new(0),
            resolver_errors: AtomicUsize::new(0),
            diagnostics: Mutex::new(ResolverDiagnostics::default()),
        };
        if let Some(error) = error {
            canonicalizer.record_error(None, &cells.root().display().to_string(), &error);
        }
        Ok(canonicalizer)
    }

    pub fn stats(&self) -> CanonicalizationStats {
        CanonicalizationStats {
            labels_rewritten: self.labels_rewritten.load(Ordering::Relaxed),
            packages_resolved_through_symlink: self
                .packages_resolved_through_symlink
                .load(Ordering::Relaxed),
            resolver_errors: self.resolver_errors.load(Ordering::Relaxed),
        }
    }

    /// Returns a replacement only when a target or package label changes.
    pub fn canonical_label(&self, label: &str) -> Option<String> {
        let root = self.root.as_ref()?;
        if !label.contains("//") {
            return None;
        }
        let package_end = label
            .find(':')
            .unwrap_or_else(|| label.strip_suffix("/...").map_or(label.len(), str::len));
        let package = &label[..package_end];
        if !package.contains("//") {
            return None;
        }
        let canonical = match self.packages.get(package) {
            Some(entry) => entry.clone(),
            None => self
                .packages
                .entry(package.to_owned())
                .or_insert_with(|| match self.canonical_package(root, package) {
                    Ok(Some(canonical)) => {
                        self.packages_resolved_through_symlink
                            .fetch_add(1, Ordering::Relaxed);
                        Some(canonical)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        let path = self
                            .cells
                            .resolve(&CellPath::new(package))
                            .map(|relative| root.join(relative.as_str()).display().to_string())
                            .unwrap_or_else(|_| package.to_owned());
                        self.record_error(Some(package), &path, &error);
                        None
                    }
                })
                .clone(),
        };
        canonical.map(|canonical| {
            self.labels_rewritten.fetch_add(1, Ordering::Relaxed);
            format!("{canonical}{}", &label[package_end..])
        })
    }

    pub fn canonicalize(&self, label: &mut String) {
        if let Some(canonical) = self.canonical_label(label) {
            *label = canonical;
        }
    }

    fn record_error(&self, package: Option<&str>, path: &str, error: &anyhow::Error) {
        self.resolver_errors.fetch_add(1, Ordering::Relaxed);
        let kind = error
            .downcast_ref::<std::io::Error>()
            .map(|error| error.kind());
        let path = path.to_owned();
        let mut diagnostics = self.diagnostics.lock().expect("resolver diagnostics lock");
        let kind_name = kind.map_or_else(|| "NonIo".to_owned(), |kind| format!("{kind:?}"));
        *diagnostics.kinds.entry(kind_name).or_default() += 1;
        if diagnostics.samples.len() < RESOLVER_ERROR_SAMPLE_LIMIT
            && !diagnostics.samples.contains(&(kind, path.clone()))
        {
            if let Some(package) = package {
                warn!(?kind, %path, package, error = %format_args!("{error:#}"), "Package canonicalization failed; preserving label");
            } else {
                warn!(?kind, %path, error = %format_args!("{error:#}"), "Canonicalization failed; preserving labels");
            }
            diagnostics.samples.push((kind, path));
        }
    }

    fn canonical_package(&self, root: &Path, package: &str) -> anyhow::Result<Option<String>> {
        let relative = self.cells.resolve(&CellPath::new(package))?;
        let absolute = root.join(relative.as_str());
        let canonical = canonicalize_existing_prefix(&absolute)?;
        if canonical == absolute {
            return Ok(None);
        }
        let Ok(relative) = canonical.strip_prefix(root) else {
            return Ok(None);
        };
        let relative = relative
            .to_str()
            .context("Canonical package path is not UTF-8")?;
        let canonical = self.cells.unresolve(&ProjectRelativePath::new(relative))?;
        Ok(Some(canonical.as_str().to_owned()))
    }
}

impl Drop for PackageCanonicalizer<'_> {
    fn drop(&mut self) {
        if self.resolver_errors.load(Ordering::Relaxed) > 0 {
            let diagnostics = self.diagnostics.lock().expect("resolver diagnostics lock");
            info!(resolver_error_kinds = ?diagnostics.kinds, "Package canonicalization error breakdown");
        }
    }
}

// Removed packages still need the identity of their surviving directory ancestors.
fn canonicalize_existing_prefix(path: &Path) -> anyhow::Result<PathBuf> {
    let mut prefix = path;
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(prefix) {
            Ok(mut canonical) => {
                for component in missing.into_iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::read_link(prefix) {
                    Ok(destination) => {
                        let destination = if destination.is_absolute() {
                            destination
                        } else {
                            prefix
                                .parent()
                                .context("Symlink has no parent directory")?
                                .join(destination)
                        };
                        let mut canonical = canonicalize_existing_prefix(&destination)?;
                        for component in missing.into_iter().rev() {
                            canonical.push(component);
                        }
                        return Ok(canonical);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::NotFound | ErrorKind::InvalidInput
                        ) => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("Reading package symlink {}", prefix.display())
                        });
                    }
                }
                missing.push(
                    prefix
                        .file_name()
                        .with_context(|| format!("No existing ancestor for {}", path.display()))?,
                );
                prefix = prefix
                    .parent()
                    .with_context(|| format!("No existing ancestor for {}", path.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Canonicalizing package directory {}", path.display())
                });
            }
        }
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    #[test]
    fn preserves_all_labels_when_root_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("missing");
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        for input in [
            "code//alias/pkg:lib",
            "code//alias/...",
            "root//:lib",
            "unknown//pkg:lib",
            "//...",
            "plain",
        ] {
            assert_eq!(canonicalizer.canonical_label(input), None);
            let mut label = input.to_owned();
            canonicalizer.canonicalize(&mut label);
            assert_eq!(label, input);
        }
        assert_eq!(canonicalizer.stats().resolver_errors, 1);
        assert_eq!(canonicalizer.stats().labels_rewritten, 0);
        assert_eq!(canonicalizer.stats().packages_resolved_through_symlink, 0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn preserves_labels_when_a_path_component_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code")).unwrap();
        fs::write(root.join("code/blocked"), "file").unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = "code//blocked/pkg:lib".to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, "code//blocked/pkg:lib");
        assert_eq!(canonicalizer.stats().resolver_errors, 1);
        canonicalizer.canonicalize(&mut label);
        assert_eq!(canonicalizer.stats().resolver_errors, 1);
        assert_eq!(canonicalizer.stats().labels_rewritten, 0);
    }

    #[test]
    fn bounds_error_samples_without_losing_counts() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("code")).unwrap();
        fs::write(tmp.path().join("code/blocked"), "file").unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": tmp.path(), "code": tmp.path().join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        for index in 0..=RESOLVER_ERROR_SAMPLE_LIMIT {
            let mut label = format!("code//blocked/pkg{index}:lib");
            canonicalizer.canonicalize(&mut label);
            assert_eq!(label, format!("code//blocked/pkg{index}:lib"));
        }
        assert_eq!(
            canonicalizer.stats().resolver_errors,
            RESOLVER_ERROR_SAMPLE_LIMIT + 1
        );
        let diagnostics = canonicalizer.diagnostics.lock().unwrap();
        assert_eq!(diagnostics.samples.len(), RESOLVER_ERROR_SAMPLE_LIMIT);
        assert_eq!(
            diagnostics.kinds["NotADirectory"],
            RESOLVER_ERROR_SAMPLE_LIMIT + 1
        );
    }

    #[rstest::rstest]
    #[case("code//alias/pkg:lib", "code//real/pkg:lib")]
    #[case("code//chain/pkg:lib", "code//real/pkg:lib")]
    #[case("code//alias/pkg:", "code//real/pkg:")]
    #[case("code//alias/...", "code//real/...")]
    #[case("code//alias/pkg", "code//real/pkg")]
    #[case("code//alias/missing:lib", "code//real/missing:lib")]
    #[case("code//alias_extra/pkg:lib", "code//alias_extra/pkg:lib")]
    #[case("root//code/real/pkg:lib", "root//code/real/pkg:lib")]
    #[case("code//other/pkg:lib", "code//other/pkg:lib")]
    #[case("code//...", "code//...")]
    #[case("//...", "//...")]
    #[case("code//:lib", "code//:lib")]
    fn resolves_directory_symlinks(#[case] input: &str, #[case] expected: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code/real/pkg")).unwrap();
        symlink("real", root.join("code/alias")).unwrap();
        symlink("alias", root.join("code/chain")).unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = input.to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, expected);
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, expected);
        assert_eq!(canonicalizer.stats().resolver_errors, 0);
    }

    #[test]
    fn resolves_symlinks_across_cells() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code")).unwrap();
        fs::create_dir_all(root.join("nested/pkg")).unwrap();
        symlink("../nested", root.join("code/alias")).unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code"), "nested": root.join("nested")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = "code//alias/pkg:lib".to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, "nested//pkg:lib");
    }

    #[test]
    fn cell_roots_match_directory_components() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code")).unwrap();
        fs::create_dir_all(root.join("code_extra/pkg")).unwrap();
        symlink("../code_extra", root.join("code/alias")).unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = "code//alias/pkg:lib".to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, "root//code_extra/pkg:lib");
    }

    #[test]
    fn does_not_map_paths_outside_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code")).unwrap();
        symlink(external.path(), root.join("code/alias")).unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = "code//alias:lib".to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, "code//alias:lib");
    }

    #[test]
    fn preserves_labels_when_symlinks_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code")).unwrap();
        symlink("alias", root.join("code/alias")).unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = "code//alias:lib".to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, "code//alias:lib");
        assert_eq!(canonicalizer.stats().resolver_errors, 1);
    }

    #[test]
    fn resolves_aliases_when_the_destination_directory_was_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("code")).unwrap();
        symlink("removed", root.join("code/alias")).unwrap();
        let cells = CellInfo::parse(
            &serde_json::json!({"root": root, "code": root.join("code")}).to_string(),
        )
        .unwrap();
        let canonicalizer = PackageCanonicalizer::new(&cells).unwrap();
        let mut label = "code//alias/pkg:lib".to_owned();
        canonicalizer.canonicalize(&mut label);
        assert_eq!(label, "code//removed/pkg:lib");
    }
}
