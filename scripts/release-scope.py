#!/usr/bin/env python3
"""Parse and validate release-scope.toml; render the release gate inputs.

The release workflow uses this as the single source of truth:

  matrix  -> GitHub Actions output lines for the `real-backends` matrix
  notes   -> the support-matrix section embedded in the GitHub release
  check   -> schema validation (CI job, and before every release)

See release-scope.toml and docs/testing/real-backends.md for the policy:
a provider is only claimed when its suite is declared `supported` AND the
release workflow's real-backends gate ran and passed. `not-run` providers
are explicit non-claims, never silent passes.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_SCOPE = REPO_ROOT / "release-scope.toml"
KNOWN_SUITES = {"lvm", "ceph", "drbd"}
KNOWN_STATUSES = {"supported", "not-run"}
PROVIDER_NAME = re.compile(r"^[a-z0-9][a-z0-9-]*$")
TOP_LEVEL_KEYS = {"schema", "providers"}
PROVIDER_KEYS = {"status", "suite", "claim", "reason"}


class ScopeError(Exception):
    """A release-scope.toml schema/policy violation."""


def load(path: Path) -> dict:
    if not path.is_file():
        raise ScopeError(f"release scope file not found: {path}")
    try:
        with path.open("rb") as handle:
            data = tomllib.load(handle)
    except tomllib.TOMLDecodeError as error:
        raise ScopeError(f"{path} is not valid TOML: {error}") from error

    unknown = set(data) - TOP_LEVEL_KEYS
    if unknown:
        raise ScopeError(f"{path}: unknown top-level key(s): {', '.join(sorted(unknown))}")
    if data.get("schema") != 1:
        raise ScopeError(f"{path}: expected schema = 1, found {data.get('schema')!r}")
    providers = data.get("providers")
    if not isinstance(providers, dict) or not providers:
        raise ScopeError(f"{path}: [providers.*] must declare at least one provider")

    seen_suites: dict[str, str] = {}
    for name, entry in providers.items():
        if not PROVIDER_NAME.fullmatch(name):
            raise ScopeError(f"{path}: provider name {name!r} must match {PROVIDER_NAME.pattern}")
        if not isinstance(entry, dict):
            raise ScopeError(f"{path}: providers.{name} must be a table")
        unknown = set(entry) - PROVIDER_KEYS
        if unknown:
            raise ScopeError(
                f"{path}: providers.{name} has unknown key(s): {', '.join(sorted(unknown))}"
            )
        status = entry.get("status")
        if status not in KNOWN_STATUSES:
            raise ScopeError(
                f"{path}: providers.{name}.status must be one of "
                f"{sorted(KNOWN_STATUSES)}, found {status!r}"
            )
        suite = entry.get("suite")
        if suite not in KNOWN_SUITES:
            raise ScopeError(
                f"{path}: providers.{name}.suite must be one of "
                f"{sorted(KNOWN_SUITES)}, found {suite!r}"
            )
        claim = entry.get("claim", "")
        reason = entry.get("reason", "")
        if not isinstance(claim, str) or not isinstance(reason, str):
            raise ScopeError(f"{path}: providers.{name}.claim/reason must be strings")
        if status == "supported":
            if not claim.strip():
                raise ScopeError(
                    f"{path}: providers.{name} is supported but has no non-empty claim"
                )
            if reason.strip():
                raise ScopeError(
                    f"{path}: providers.{name} is supported; remove the `reason` "
                    "(that field is for explicit not-run non-claims)"
                )
        else:
            if not reason.strip():
                raise ScopeError(
                    f"{path}: providers.{name} is not-run but has no reason; "
                    "an unexplained non-claim is not acceptable release evidence"
                )
            if claim.strip():
                raise ScopeError(
                    f"{path}: providers.{name} is not-run; remove the `claim` "
                    "(a not-run provider is explicitly not claimed)"
                )
        if suite in seen_suites:
            raise ScopeError(
                f"{path}: providers.{name} and providers.{seen_suites[suite]} "
                f"both use suite {suite!r}; one suite cannot prove two independent claims"
            )
        seen_suites[suite] = name

    return data


def supported_suites(data: dict) -> list[str]:
    suites = [
        entry["suite"]
        for entry in data["providers"].values()
        if entry["status"] == "supported"
    ]
    return sorted(suites)


def cmd_matrix(data: dict) -> int:
    suites = supported_suites(data)
    count = len(suites)
    # GitHub cannot expand an empty matrix. "none" is skipped by the
    # release workflow's job condition; supported_count remains 0 so the
    # gate aggregator knows that is intentional, not a missing gate.
    print(f"matrix={json.dumps({'suite': suites or ['none']}, separators=(',', ':'))}")
    print(f"supported_count={count}")
    print(f"supported_providers={','.join(name for name, entry in data['providers'].items() if entry['status'] == 'supported')}")
    return 0


def cmd_notes(data: dict, sha: str, gate_state: str) -> int:
    print("## Provider release scope")
    print()
    print(
        "Generated from `release-scope.toml` at commit "
        f"`{sha}`. `supported` means this release ran the provider's "
        "real-backend suite and refuses to publish unless it passed. "
        "`not-run` means this release makes **no claim** for that provider."
    )
    print()
    print("| Provider | Release scope | Suite | Evidence / non-claim reason |")
    print("|---|---|---|---|")
    for name, entry in data["providers"].items():
        status = entry["status"]
        suite = entry["suite"]
        if status == "supported":
            if gate_state == "passed":
                evidence = (
                    f"real-backends ({suite}) **passed** at `{sha}`; "
                    "the publish job depends on this gate"
                )
            elif gate_state == "failed":
                evidence = (
                    f"real-backends ({suite}) **did not pass**; publish is blocked"
                )
            else:
                evidence = (
                    f"real-backends ({suite}) is required at `{sha}`; "
                    "the publish job only runs after it passes"
                )
        else:
            evidence = f"**not claimed** — {entry['reason']}"
        print(f"| `{name}` | `{status}` | `{suite}` | {evidence} |")
    print()
    print(
        "This table is a release-scope record, not a production-support claim "
        "(AGENTS rule 12). A not-run provider has no release claim and no "
        "suite result to cite."
    )
    return 0


def cmd_check(data: dict) -> int:
    providers = data["providers"]
    supported = [name for name, entry in providers.items() if entry["status"] == "supported"]
    not_run = [name for name, entry in providers.items() if entry["status"] == "not-run"]
    print(
        f"release scope OK: {len(providers)} provider(s); "
        f"supported={supported or '[]'}; not-run={not_run or '[]'}"
    )
    for suite in supported_suites(data):
        print(f"  gate required: suite {suite}")
    return 0


def cmd_status(data: dict) -> int:
    """Emit suite=status lines for GitHub Actions outputs (schedule selection)."""
    by_suite = {
        entry["suite"]: entry["status"]
        for entry in data["providers"].values()
    }
    for suite in sorted(KNOWN_SUITES):
        print(f"{suite}={by_suite.get(suite, 'not-run')}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--file", type=Path, default=DEFAULT_SCOPE)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("check", help="validate the scope file")
    sub.add_parser("matrix", help="emit GitHub Actions matrix outputs")
    sub.add_parser("status", help="emit suite=supported|not-run GitHub Actions outputs")
    notes = sub.add_parser("notes", help="render the release-notes support matrix")
    notes.add_argument("--sha", required=True)
    notes.add_argument(
        "--gate-state",
        choices=["required", "passed", "failed"],
        default="required",
        help="the real-backends gate state in the release run that renders these notes",
    )

    args = parser.parse_args()
    try:
        data = load(args.file)
        if args.command == "check":
            return cmd_check(data)
        if args.command == "matrix":
            return cmd_matrix(data)
        if args.command == "status":
            return cmd_status(data)
        return cmd_notes(data, args.sha, args.gate_state)
    except ScopeError as error:
        print(f"release-scope: ERROR: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
