#!/usr/bin/env python3
"""Convert cargo/libtest output into a JUnit XML evidence record.

This is deliberately a *record* of what happened, never a substitute for
the process exit code: the calling script fails on the test process's own
status. The converter exists so the scheduled/release workflows can upload
JUnit alongside the raw log without adding a test-runner dependency.

Input:
  --log      cargo test output (may be absent when preflight failed)
  --errors   typed gate errors written by real-backends-common.sh
  --suite    provider/suite name, used in the testsuite name
  --exit-code process exit code (a non-zero code with no failed testcase
               becomes a synthetic `cargo-test` error)
  --timestamp ISO-8601 UTC start time recorded by the script
"""

from __future__ import annotations

import argparse
import re
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

TEST_LINE = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored|bench)")
RUNNING_LINE = re.compile(r"^\s*Running (?:unittests )?(.+?)(?: \(.*\))?$")
DOCTEST_LINE = re.compile(r"^\s*Doc-tests (.+?)$")
RESULT_LINE = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; "
    r"(\d+) ignored; (\d+) measured; (\d+) filtered out; finished in ([\d.]+)s"
)
CONTROL_CHARS = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f]")


@dataclass
class Case:
    name: str
    status: str


@dataclass
class Suite:
    name: str
    cases: list[Case] = field(default_factory=list)
    duration: float = 0.0
    ignored: int = 0


def clean(text: str) -> str:
    return CONTROL_CHARS.sub("", text)


def parse_log(log_path: Path) -> list[Suite]:
    suites: list[Suite] = []
    current = Suite(name="cargo-test")
    saw_running = False
    for raw in log_path.read_text(encoding="utf-8", errors="replace").splitlines():
        running = RUNNING_LINE.match(raw)
        doctest = DOCTEST_LINE.match(raw)
        if running:
            if current.cases or saw_running:
                suites.append(current)
            current = Suite(name=running.group(1).strip())
            saw_running = True
            continue
        if doctest:
            if current.cases or saw_running:
                suites.append(current)
            current = Suite(name=f"doc-tests {doctest.group(1).strip()}")
            saw_running = True
            continue
        test = TEST_LINE.match(raw)
        if test:
            current.cases.append(Case(name=test.group(1).strip(), status=test.group(2)))
            continue
        result = RESULT_LINE.match(raw)
        if result:
            current.duration = float(result.group(7))
            current.ignored = int(result.group(4))
            suites.append(current)
            current = Suite(name="cargo-test")
            saw_running = False
    if current.cases:
        suites.append(current)
    return suites


def add_preflight_failure(suites: list[Suite], errors_path: Path) -> None:
    if not errors_path.exists():
        return
    lines = [line for line in errors_path.read_text(encoding="utf-8", errors="replace").splitlines() if line.strip()]
    if not lines:
        return
    suite = Suite(name="gate-preflight")
    for line in lines:
        code, _, message = line.partition("\t")
        case = Case(name=f"preflight:{code or 'unknown'}", status="FAILED")
        suite.cases.append(case)
        # The failure body is attached during rendering below.
        if not hasattr(suite, "messages"):
            suite.messages = {}  # type: ignore[attr-defined]
        suite.messages[case.name] = message or line  # type: ignore[attr-defined]
    suites.insert(0, suite)


def build_xml(suites: list[Suite], suite_name: str, exit_code: int, timestamp: str) -> ET.ElementTree:
    total_tests = 0
    total_failures = 0
    total_skipped = 0
    total_time = 0.0
    for suite in suites:
        for case in suite.cases:
            total_tests += 1
            if case.status == "FAILED":
                total_failures += 1
            elif case.status in ("ignored", "bench"):
                total_skipped += 1
        total_time += suite.duration

    if exit_code != 0 and total_failures == 0:
        # A non-zero cargo/test exit with no FAILED testcase (compile
        # error, harness abort, no tests matched) is a gate error, not a
        # pass and not a silent empty suite.
        suites.append(
            Suite(
                name="cargo-test",
                cases=[Case(name="cargo-test-exit", status="FAILED")],
            )
        )
        total_tests += 1
        total_failures += 1

    root = ET.Element(
        "testsuites",
        {
            "name": f"volvisor-real-backends.{suite_name}",
            "tests": str(total_tests),
            "failures": str(total_failures),
            "errors": "0",
            "skipped": str(total_skipped),
            "time": f"{total_time:.3f}",
        },
    )
    for suite in suites:
        failures = sum(1 for case in suite.cases if case.status == "FAILED")
        skipped = sum(1 for case in suite.cases if case.status in ("ignored", "bench"))
        element = ET.SubElement(
            root,
            "testsuite",
            {
                "name": clean(suite.name),
                "tests": str(len(suite.cases)),
                "failures": str(failures),
                "errors": "0",
                "skipped": str(skipped),
                "time": f"{suite.duration:.3f}",
                "timestamp": timestamp,
            },
        )
        messages = getattr(suite, "messages", {})
        for case in suite.cases:
            case_element = ET.SubElement(
                element,
                "testcase",
                {
                    "classname": clean(suite_name),
                    "name": clean(case.name),
                    "time": "0.000",
                },
            )
            if case.status == "FAILED":
                failure = ET.SubElement(
                    case_element,
                    "failure",
                    {"message": "real-backend gate did not pass; see run.log"},
                )
                body = messages.get(case.name, "")
                failure.text = clean(body or "libtest reported FAILED; the raw log is the evidence.")
            elif case.status in ("ignored", "bench"):
                ET.SubElement(case_element, "skipped", {"message": "reported by libtest"})
    return ET.ElementTree(root)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", required=True)
    parser.add_argument("--log", required=True)
    parser.add_argument("--errors", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--exit-code", required=True, type=int)
    parser.add_argument("--timestamp", default="")
    args = parser.parse_args()

    timestamp = args.timestamp or datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    log_path = Path(args.log)
    suites = parse_log(log_path) if log_path.exists() else []
    add_preflight_failure(suites, Path(args.errors))
    tree = build_xml(suites, args.suite, args.exit_code, timestamp)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    tree.write(out, encoding="utf-8", xml_declaration=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
