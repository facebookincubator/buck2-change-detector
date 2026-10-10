# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This source code is dual-licensed under either the MIT license found in the
# LICENSE-MIT file in the root directory of this source tree or the Apache
# License, Version 2.0 found in the LICENSE-APACHE file in the root directory
# of this source tree. You may select, at your option, one of the
# above-listed licenses.

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest


@pytest.mark.parametrize(
    "phase,expected_error",
    [
        ("public", "reading graph metadata"),
        ("draft", None),
        ("secret", None),
        ("missing", "unknown revision"),
        ("unexpected", "unexpected commit phase"),
        ("malformed", "expected a 40-character hexadecimal commit hash"),
        ("skip", None),
    ],
)
def test_graph_upload_validates_base_without_checkout(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    phase: str,
    expected_error: str | None,
) -> None:
    base = "." if phase == "malformed" else "a" * 40
    head = "b" * 40
    # Mock SCM, not the publisher. A public base must reach graph validation
    # even though the working copy is a different, non-public revision.
    sl = tmp_path / "sl"
    sl.write_text(
        f"#!{sys.executable}\n"
        "import sys\n"
        f"open({str(tmp_path / 'scm_called')!r}, 'w').close()\n"
        "args = sys.argv[1:]\n"
        "rev = args[args.index('-r') + 1]\n"
        f"if rev == '.':\n    print({head!r})\n"
        "elif rev == '. & public()':\n    pass\n"
        f"elif rev == {base!r}:\n"
        + (
            "    sys.exit('unknown revision')\n"
            if phase == "missing"
            else f"    print({phase!r})\n"
        )
        + "else:\n    sys.exit('unexpected SCM query: ' + repr(args))\n"
    )
    sl.chmod(0o755)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    preparation = tmp_path / "preparation.json"
    preparation.write_text(
        json.dumps(
            {
                "schema_version": 2,
                "requested_commit": base,
                "publication": {"type": "skip", "reason": "exact_cache_hit"}
                if phase == "skip"
                else {
                    "type": "upload",
                    "version": "test-version",
                    "reason": "full_computation_required",
                },
            }
        )
    )
    result = subprocess.run(
        [
            os.environ["SUPERTD"],
            "graph",
            "upload",
            "--graph",
            str(tmp_path / "missing.bin"),
            "--preparation-result",
            str(preparation),
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if expected_error is None:
        assert result.returncode == 0, result.stderr
        assert (
            "exact_cache_hit" if phase == "skip" else "commit_not_public"
        ) in result.stderr
    else:
        assert result.returncode != 0
        assert expected_error in result.stderr
    assert "graph upload commit mismatch" not in result.stderr
    if phase in {"skip", "malformed"}:
        assert not (tmp_path / "scm_called").exists()
