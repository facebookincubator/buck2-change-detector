# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is licensed under both the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree and the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree.

# pyre-unsafe

import glob
import json
import os
import shutil
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

import pytest


def run(*args, output=None, log_output=None, expect_fail=None):
    # On Ci stderr gets out of order with stdout. To avoid this, we need to flush stdout/stderr first.
    sys.stdout.flush()
    sys.stderr.flush()
    try:
        result = subprocess.run(
            tuple(args), check=True, capture_output=True, encoding="utf-8", timeout=30
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


def get_patches():
    patches = glob.glob(os.getenv("TESTCASES") + "/*.patch")
    assert patches != []
    test_names = [os.path.basename(patch) for patch in patches]
    return test_names


@pytest.mark.parametrize("patch_name", get_patches())
def test_run(patch_name):
    audit = os.getenv("AUDIT")
    btd = os.getenv("BTD")
    buck = os.getenv("BUCK")
    base = os.getenv("BASE")
    targets = os.getenv("TARGETS")
    patch = os.path.join(os.getenv("TESTCASES"), patch_name)
    patch_name = Path(patch).stem

    btd_fail = expect_btd_failure(patch_name)

    with tempfile.TemporaryDirectory() as working_dir, tempfile.TemporaryDirectory() as output_dir:
        os.chdir(working_dir)
        out_base = Path(output_dir).joinpath("base.jsonl")
        out_diff = Path(output_dir).joinpath("diff.jsonl")
        out_cells = Path(output_dir).joinpath("cells.json")
        out_config = Path(output_dir).joinpath("config.json")
        out_changes = Path(output_dir).joinpath("changes.txt")
        out_btd1 = Path(output_dir).joinpath("btd1.json")
        out_btd2 = Path(output_dir).joinpath("btd2.json")
        out_targets = Path(output_dir).joinpath("targets.txt")
        out_rerun = Path(output_dir).joinpath("rerun.txt")
        btd_args = [
            "--check-dangling",
            "--cells",
            out_cells,
            "--config",
            out_config,
            "--changes",
            out_changes,
            "--base",
            out_base,
        ]

        run("hg", "init")
        apply(base, None)
        run("hg", "add")
        run("hg", "commit", "--message=wibble")
        run(
            targets,
            "--buck",
            buck,
            "--output",
            out_base,
            "root//...",
            log_output=out_base,
        )
        apply(base, patch)
        run(audit, "cell", "--buck", buck, output=out_cells)
        run(audit, "config", "--buck", buck, output=out_config)
        run(
            targets,
            "--buck",
            buck,
            "--output",
            out_diff,
            "root//...",
            log_output=out_diff,
        )
        run("hg", "status", "-amr", "--root-relative", output=out_changes)
        run(
            btd,
            *btd_args,
            "--diff",
            out_diff,
            "--json",
            output=out_btd1,
            expect_fail=btd_fail,
        )
        run(
            btd,
            *btd_args,
            "--universe",
            "root//...",
            "--buck",
            buck,
            "--json",
            output=out_btd2,
            expect_fail=btd_fail,
        )
        run(
            btd,
            *btd_args,
            "--universe",
            "root//...",
            "--buck",
            buck,
            "--print-rerun",
            output=out_rerun,
        )
        if not btd_fail:
            # We want to make sure our rerun logic was correct
            assert read_file(out_btd1) == read_file(out_btd2)
            # And that we have valid JSON results
            output = json.loads(read_file(out_btd1))
            # And that they can build
            with open(out_targets, "w") as file:
                for x in output:
                    file.write(x["target"] + "\n")
            run(buck, "build", "@" + str(out_targets))
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
                "root_cause": ["root//inner:baz", "inputs"],
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
                "root_cause": ["root//inner:baz", "hash"],
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
                    "root_cause": ["root//inner:baz", "package_values"],
                    "is_terminal": False,
                },
            }
        ]
    elif patch == "new_buck":
        assert len(rdeps) == 1
        assert rdeps[0]["target"] == "root//new:target"
    elif patch == "new_outside_universe" or patch == "new_ignored":
        assert rdeps == []
    else:
        raise AssertionError("No properties known for: " + patch)


def check_properties_rerun(patch, rerun):
    if patch == "nothing":
        assert rerun == ""
    elif patch == "file":
        assert rerun == ""
    elif patch == "rename_inner":
        assert rerun == "+ root//inner\n"
    elif patch == "buckconfig":
        assert rerun == "* everything\n"
    elif patch == "cfg_modifiers":
        assert rerun == "+ root//inner\n"
    elif patch == "delete_inner":
        assert rerun == "- root//inner\n"
    elif patch == "new_buck":
        assert rerun == "+ root//\n+ root//new\n"
    elif patch == "new_outside_universe" or patch == "new_ignored":
        assert rerun == ""
    else:
        raise AssertionError("No properties known for: " + patch)


def expect_btd_failure(patch):
    if patch == "delete_inner":
        return "Target `root//inner:baz` was deleted but is referenced by `root//:bar`"
    else:
        return None
