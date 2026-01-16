# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

# pyre-unsafe

import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

import pytest


@dataclass
class EnvConfig:
    audit: str
    btd: str
    buck: str
    base: str
    targets: str
    testcases: str

    @classmethod
    def from_env(cls) -> "EnvConfig":
        return cls(
            audit=os.getenv("AUDIT", ""),
            btd=os.getenv("BTD", ""),
            buck=os.getenv("BUCK", ""),
            base=os.getenv("BASE", ""),
            targets=os.getenv("TARGETS", ""),
            testcases=os.getenv("TESTCASES", ""),
        )


@dataclass
class OutputPaths:
    base: Path
    diff: Path
    cells: Path
    config: Path
    changes: Path
    btd_dangling_errors: Path

    @classmethod
    def create(cls, output_dir: Path) -> "OutputPaths":
        return cls(
            base=output_dir / "base.jsonl",
            diff=output_dir / "diff.jsonl",
            cells=output_dir / "cells.json",
            config=output_dir / "config.json",
            changes=output_dir / "changes.txt",
            btd_dangling_errors=output_dir / "btd_dangling_errors.json",
        )

    def btd_args(self) -> list:
        return [
            "--check-dangling",
            "--check-dangling-universe=root//...",
            "--write-dangling-errors-to-file",
            self.btd_dangling_errors,
            "--cells",
            self.cells,
            "--config",
            self.config,
            "--changes",
            self.changes,
            "--base",
            self.base,
        ]


def run(*args, output=None, log_output=None, expect_fail=None):
    # On Ci stderr gets out of order with stdout. To avoid this, we need to flush stdout/stderr first.
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        result = subprocess.run(
            tuple(args),
            check=True,
            env={
                **os.environ,
                "CHGDISABLE": "1",  # Avoid spawning long-lived hg processes.
            },
            capture_output=True,
            encoding="utf-8",
            timeout=30,
        )
        if output:
            write_file(output, result.stdout)
        if expect_fail is not None:
            raise RuntimeError("Expected exception but didn't fail")
    except subprocess.CalledProcessError as e:
        if expect_fail is not None and expect_fail in e.stderr:
            return
        print("PROCESS FAILED")
        print("RAN: " + repr(args))
        print("STDOUT: " + e.stdout)
        print("STDERR: " + e.stderr)
        if log_output is not None:
            print("LOG: " + read_file(log_output))
        if expect_fail is not None:
            print("EXPECT FAIL: " + expect_fail)
        print("DONE")
        # No stack trace on SIGINT
        sys.exit(1)


def write_file(path, contents):
    with open(path, "w") as file:
        file.write(contents)


def read_file(path):
    with open(path, "r") as file:
        return file.read()


def copy_tree(src, dst):
    if not os.path.exists(dst):
        os.makedirs(dst)
    for item in os.listdir(src):
        s = os.path.join(src, item)
        d = os.path.join(dst, item)
        if os.path.isdir(s):
            copy_tree(s, d)
        else:
            shutil.copy2(s, d.removesuffix(".test"))


def rmtree_with_retry(path, retry=3):
    for _ in range(retry):
        try:
            shutil.rmtree(path)
            return
        except OSError:
            time.sleep(1)


def apply(base, patch):
    for file in glob.glob("*"):
        if file != ".hg" and os.path.isfile(file):
            os.remove(file)
    copy_tree(base, ".")
    # So there is always something to commit
    with open("stamp.txt", "w") as file:
        file.write(str(uuid.uuid4()))
    run("hg", "addremove")
    run("hg", "commit", "--message=temp")
    if patch is not None:
        run("hg", "import", "--no-commit", patch)
    run("hg", "uncommit")


@contextmanager
def test_environment():
    """Context manager that sets up temp directories and cleans up after."""
    with (
        tempfile.TemporaryDirectory() as working_dir,
        tempfile.TemporaryDirectory() as output_dir,
    ):
        os.chdir(working_dir)
        paths = OutputPaths.create(Path(output_dir))
        try:
            yield Path(output_dir), paths
        finally:
            rmtree_with_retry(working_dir)


def setup_base_repo(env: EnvConfig, paths: OutputPaths):
    """Initialize hg repo and create base commit with targets."""
    run("hg", "init")
    apply(env.base, None)
    run("hg", "add")
    run("hg", "commit", "--message=wibble")
    run(
        env.targets,
        "--buck",
        env.buck,
        "--output",
        paths.base,
        "root//...",
        log_output=paths.base,
    )


def apply_patch_and_collect(env: EnvConfig, paths: OutputPaths, patch: str):
    """Apply a patch and collect cell/config/diff/changes info."""
    apply(env.base, patch)
    run(env.audit, "cell", "--buck", env.buck, output=paths.cells)
    run(env.audit, "config", "--buck", env.buck, output=paths.config)
    run(
        env.targets,
        "--buck",
        env.buck,
        "--output",
        paths.diff,
        "root//...",
        log_output=paths.diff,
    )
    run("hg", "status", "-amr", "--root-relative", output=paths.changes)


def get_patches():
    patches = glob.glob(os.getenv("TESTCASES") + "/*.patch")
    assert patches != []
    test_names = [os.path.basename(patch) for patch in patches]
    return test_names


@pytest.mark.parametrize("patch_name", get_patches())
def test_run(patch_name):
    env = EnvConfig.from_env()
    patch = os.path.join(env.testcases, patch_name)
    patch_name = Path(patch).stem

    with test_environment() as (output_dir, paths):
        out_btd1 = output_dir / "btd1.json"
        out_btd2 = output_dir / "btd2.json"
        out_targets = output_dir / "targets.txt"
        out_rerun = output_dir / "rerun.txt"

        setup_base_repo(env, paths)
        apply_patch_and_collect(env, paths, patch)

        btd_args = paths.btd_args()
        run(
            env.btd,
            *btd_args,
            "--diff",
            paths.diff,
            "--json",
            output=out_btd1,
        )
        run(
            env.btd,
            *btd_args,
            "--universe",
            "root//...",
            "--buck",
            env.buck,
            "--json",
            output=out_btd2,
        )
        run(
            env.btd,
            *btd_args,
            "--universe",
            "root//...",
            "--buck",
            env.buck,
            "--print-rerun",
            output=out_rerun,
        )
        # We want to make sure our rerun logic was correct
        assert read_file(out_btd1) == read_file(out_btd2)
        # And that we have valid JSON results
        output = json.loads(read_file(out_btd1))

        # For delete_inner, check the dangling errors
        if expect_dangling_check_error(patch_name):
            assert_dangling_check_errors(paths.btd_dangling_errors)
        else:
            # And that they can build
            with open(out_targets, "w") as file:
                for x in output:
                    file.write(x["target"] + "\n")
            run(env.buck, "build", "@" + str(out_targets))
            # Check custom properties
            check_properties(patch_name, output)
        rerun = read_file(out_rerun)
        check_properties_rerun(patch_name, rerun)


def check_properties(patch, rdeps):
    if patch == "nothing":
        assert rdeps == []
    elif patch == "file":
        assert {
            "depth": 0,
            "labels": ["hello", "world"],
            "target": "root//inner:baz",
            "type": "my_rule",
            "oncall": None,
            "reason": {
                "affected_dep": "",
                "root_cause_target": "root//inner:baz",
                "root_cause_reason": "inputs",
                "is_terminal": False,
            },
        } in rdeps
        assert len(rdeps) == 2
    elif patch == "rename_inner":
        assert {
            "depth": 0,
            "labels": ["hello"],
            "target": "root//inner:baz",
            "type": "my_rule",
            "oncall": None,
            "reason": {
                "affected_dep": "",
                "root_cause_target": "root//inner:baz",
                "root_cause_reason": "hash",
                "is_terminal": False,
            },
        } in rdeps
        assert len(rdeps) == 2
    elif patch == "buckconfig":
        assert len(rdeps) == 3
    elif patch == "cfg_modifiers":
        assert rdeps == [
            {
                "depth": 0,
                "labels": ["hello", "world"],
                "target": "root//inner:baz",
                "type": "my_rule",
                "oncall": None,
                "reason": {
                    "affected_dep": "",
                    "root_cause_target": "root//inner:baz",
                    "root_cause_reason": "hash",
                    "is_terminal": False,
                },
            },
            {
                "depth": 1,
                "labels": [],
                "target": "root//:bar",
                "type": "my_rule",
                "oncall": None,
                "reason": {
                    "affected_dep": "root//inner:baz",
                    "root_cause_target": "root//inner:baz",
                    "root_cause_reason": "hash",
                    "is_terminal": True,
                },
            },
        ]
    elif patch == "new_buck":
        assert len(rdeps) == 1
        assert rdeps[0]["target"] == "root//new:target"
    elif patch == "new_outside_universe" or patch == "new_ignored":
        assert rdeps == []
    elif patch == "change_package_label":
        assert {
            "depth": 0,
            "labels": ["ci:package", "hello", "world"],
            "target": "root//inner:baz",
            "type": "my_rule",
            "oncall": None,
            "reason": {
                "affected_dep": "",
                "root_cause_target": "root//inner:baz",
                "root_cause_reason": "labels",
                "is_terminal": False,
                "added_labels": ["ci:package"],
            },
        } in rdeps
    elif patch == "change_package_value":
        assert {
            "depth": 0,
            "labels": ["package", "hello", "world"],
            "target": "root//inner:baz",
            "type": "my_rule",
            "oncall": None,
            "reason": {
                "affected_dep": "",
                "root_cause_target": "root//inner:baz",
                "root_cause_reason": "package_values",
                "is_terminal": False,
            },
        } in rdeps
    else:
        raise AssertionError("No properties known for: " + patch)


EXPECTED_RERUN = {
    "nothing": "",
    "file": "",
    "rename_inner": "+ root//inner\n",
    "buckconfig": "* everything\n",
    "cfg_modifiers": "+ root//inner\n",
    "delete_inner": "- root//inner\n",
    "new_buck": "+ root//\n+ root//new\n",
    "new_outside_universe": "",
    "new_ignored": "",
    "change_package_label": "+ root//inner\n",
    "change_package_value": "+ root//inner\n",
}


def check_properties_rerun(patch, rerun):
    if patch not in EXPECTED_RERUN:
        raise AssertionError("No properties known for: " + patch)
    assert rerun == EXPECTED_RERUN[patch]


def expect_dangling_check_error(patch):
    return patch == "delete_inner"


def assert_dangling_check_errors(out_btd_dangling_errors):
    assert out_btd_dangling_errors.exists()
    dangling_errors = json.loads(read_file(out_btd_dangling_errors))
    target_deleted_error = next(
        (
            error["TargetDeleted"]
            for error in dangling_errors
            if "TargetDeleted" in error
        ),
        None,
    )
    assert target_deleted_error is not None
    assert target_deleted_error["deleted"] == "root//inner:baz"
    assert target_deleted_error["referenced_by"] == "root//:bar"


def test_output_flag():
    """Test that the --output flag writes to a file instead of stdout."""
    env = EnvConfig.from_env()
    patch = os.path.join(env.testcases, "file.patch")

    with test_environment() as (output_dir, paths):
        out_btd = output_dir / "btd_output.jsonl"

        setup_base_repo(env, paths)
        apply_patch_and_collect(env, paths, patch)

        # Test --output flag: btd writes directly to file instead of stdout
        run(
            env.btd,
            *paths.btd_args(),
            "--diff",
            paths.diff,
            "--json",
            "--output",
            str(out_btd),
        )

        # Verify the output file was created and has content
        assert out_btd.exists(), f"Output file {out_btd} was not created"
        content = read_file(out_btd)
        assert len(content) > 0, "Output file is empty"

        # Verify it's valid JSON
        output = json.loads(content)
        assert isinstance(output, list), "Output should be a JSON array"
        assert len(output) == 2, "Expected 2 targets for file.patch"


def read_compressed_file(path):
    result = subprocess.run(
        ["zstd", "-d", "-c", str(path)],
        capture_output=True,
        encoding="utf-8",
        check=True,
    )
    return result.stdout


def test_compressed_output_flag():
    """Test that the --output flag with a .zst extension writes compressed output."""
    env = EnvConfig.from_env()
    patch = os.path.join(env.testcases, "file.patch")

    with test_environment() as (output_dir, paths):
        out_btd = output_dir / "btd_output.jsonl.zst"

        setup_base_repo(env, paths)
        apply_patch_and_collect(env, paths, patch)

        # Test --output flag with .zst: btd writes compressed output to file
        run(
            env.btd,
            *paths.btd_args(),
            "--diff",
            paths.diff,
            "--json",
            "--output",
            str(out_btd),
        )

        # Verify the compressed output file was created
        assert out_btd.exists(), f"Compressed output file {out_btd} was not created"

        # Verify it's a valid zstd compressed file by decompressing
        content = read_compressed_file(out_btd)
        assert len(content) > 0, "Decompressed content is empty"

        # Verify the decompressed content is valid JSON
        output = json.loads(content)
        assert isinstance(output, list), "Output should be a JSON array"
        assert len(output) == 2, "Expected 2 targets for file.patch"
