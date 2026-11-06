#!/usr/bin/env python3
"""Compare two Volvisor control-plane benchmark run directories.

The input directories are produced by ``scripts/bench/run-all.sh`` and have
this relevant layout::

    RUN/
      ENVIRONMENT.txt
      MANIFEST.sha256
      json/mixed-c1.json, json/mixed-c8.json, json/mixed-c32.json, ...
      json/soak.json

This tool verifies each run's manifest before reading anything, then emits a
CSV table and a Markdown digest. It never invents a value: an absent metric is
rendered as ``NOT RECORDED`` in Markdown and left as an empty CSV cell. These
are single-host loop-LVM control-plane measurements, not production capacity,
SLA, durability or data-path evidence.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import re
import sys
from pathlib import Path

SWEEP_TIERS = (1, 8, 32, 128)
SWEEP_SCENARIOS = tuple(f"mixed-c{tier}" for tier in SWEEP_TIERS)
SOAK_SCENARIO = "soak"

CAVEAT = (
    "Single-host loop-LVM control-plane measurements. These runs are not "
    "production capacity, SLA, durability, or data-path evidence. A dirty "
    "worktree cannot support a clean-SHA claim."
)

CONCURRENCY_METRICS = (
    ("ops_per_sec", "ops/s"),
    ("p95_ms", "p95 ms"),
    ("read_p95_ms", "read p95 ms"),
    ("write_p95_ms", "write p95 ms"),
    ("error_rate", "error rate"),
)

SOAK_PATHS = {
    "duration_seconds": ("duration_seconds",),
    "ops": ("ops",),
    "rss_first_kib": ("resources", "rss_kib", "first"),
    "rss_last_kib": ("resources", "rss_kib", "last"),
    "rss_delta_kib": ("resources", "rss_kib", "delta"),
    "journal_first_bytes": ("resources", "journal_bytes", "first"),
    "journal_last_bytes": ("resources", "journal_bytes", "last"),
    "journal_delta_bytes": ("resources", "journal_bytes", "delta"),
    "state_first_bytes": ("resources", "state_bytes", "first"),
    "state_last_bytes": ("resources", "state_bytes", "last"),
    "state_delta_bytes": ("resources", "state_bytes", "delta"),
}

SOAK_METRICS = (
    ("duration_seconds", "duration (s)"),
    ("ops", "ops"),
    ("rss_first_kib", "RSS first (KiB)"),
    ("rss_last_kib", "RSS last (KiB)"),
    ("rss_delta_kib", "RSS delta (KiB)"),
    ("journal_first_bytes", "journal first (bytes)"),
    ("journal_last_bytes", "journal last (bytes)"),
    ("journal_delta_bytes", "journal delta (bytes)"),
    ("state_first_bytes", "state first (bytes)"),
    ("state_last_bytes", "state last (bytes)"),
    ("state_delta_bytes", "state delta (bytes)"),
)

NORMALISED_METRICS = (
    ("ops_per_minute", "ops / min", "ops"),
    ("rss_delta_kib_per_minute", "RSS delta (KiB / min)", "rss_delta_kib"),
    (
        "journal_delta_bytes_per_minute",
        "journal delta (bytes / min)",
        "journal_delta_bytes",
    ),
    ("state_delta_bytes_per_minute", "state delta (bytes / min)", "state_delta_bytes"),
)

SHA256_RE = re.compile(r"[0-9a-fA-F]{64}")


class CompareError(Exception):
    """An input error that should produce a non-zero exit, not a traceback."""


class ManifestVerification:
    """Result of checking a run's MANIFEST.sha256."""

    def __init__(self, failures: list[str], verified_paths: set[str]) -> None:
        self.failures = failures
        self.verified_paths = verified_paths


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify_manifest(run_dir: Path) -> ManifestVerification:
    """Verify MANIFEST.sha256 using sha256sum -c semantics.

    Each non-empty line is ``<sha256-hex>  <relative-path>``. Paths are
    interpreted relative to ``run_dir``; absolute paths, ``..`` components and
    NUL bytes are rejected. Returns the failures and the set of verified
    relative paths (empty paths when the manifest is missing/unreadable).
    """
    manifest = run_dir / "MANIFEST.sha256"
    if not manifest.is_file():
        return ManifestVerification([f"missing {manifest}"], set())
    try:
        lines = manifest.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError as exc:
        return ManifestVerification([f"cannot read {manifest}: {exc}"], set())

    failures: list[str] = []
    verified_paths: set[str] = set()
    for line_number, raw in enumerate(lines, 1):
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            failures.append(f"line {line_number}: malformed manifest entry")
            continue
        digest, name = parts
        if not SHA256_RE.fullmatch(digest):
            failures.append(f"line {line_number}: malformed sha256 digest")
            continue
        # GNU sha256sum uses a leading '*' for binary mode.
        if name.startswith("*"):
            name = name[1:]
        if not name:
            failures.append(f"line {line_number}: empty file name")
            continue
        if "\x00" in name:
            failures.append(f"line {line_number}: malformed file name (NUL byte)")
            continue
        relative = Path(name)
        if relative.is_absolute():
            failures.append(f"line {line_number}: absolute path is not allowed: {name}")
            continue
        if ".." in relative.parts:
            failures.append(f"line {line_number}: path escapes the run directory: {name}")
            continue
        target = run_dir / relative
        try:
            actual = sha256_file(target)
        except (OSError, ValueError) as exc:
            failures.append(f"line {line_number}: FAILED open or read {name}: {exc}")
            continue
        if actual.lower() != digest.lower():
            failures.append(f"line {line_number}: FAILED checksum mismatch for {name}")
            continue
        verified_paths.add(relative.as_posix())
    return ManifestVerification(failures, verified_paths)


def discover_json_files(run_dir: Path) -> list[Path]:
    """Return the aggregate JSON files the comparison will read."""
    json_dir = run_dir / "json"
    if not json_dir.is_dir():
        raise CompareError(f"{run_dir}: missing json/ directory")
    paths = sorted(json_dir.glob("*.json"))
    if not paths:
        raise CompareError(f"{json_dir}: no aggregate JSON records")
    return paths


def manifest_covers_json(
    run_dir: Path, json_paths: list[Path], verification: ManifestVerification
) -> list[str]:
    """Failures when a JSON file being compared was not verified by the manifest."""
    failures: list[str] = []
    for path in json_paths:
        relative = path.relative_to(run_dir).as_posix()
        if relative not in verification.verified_paths:
            failures.append(
                f"{relative}: present but not listed and verified in MANIFEST.sha256"
            )

    json_dir = run_dir / "json"
    try:
        non_empty = any(json_dir.iterdir())
    except OSError as exc:
        return failures + [f"cannot read {json_dir}: {exc}"]

    verified_json = {
        path
        for path in verification.verified_paths
        if path.startswith("json/") and path.endswith(".json")
    }
    if non_empty and not verified_json:
        failures.append(
            f"{json_dir}: directory is not empty but MANIFEST.sha256 lists no JSON file"
        )
    return failures



def parse_environment(run_dir: Path) -> dict[str, str | None]:
    """Read source_sha and worktree_dirty from ENVIRONMENT.txt."""
    result: dict[str, str | None] = {"source_sha": None, "worktree_dirty": None}
    path = run_dir / "ENVIRONMENT.txt"
    if not path.is_file():
        return result
    try:
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return result
    for line in text.splitlines():
        stripped = line.strip()
        key, separator, value = stripped.partition(":")
        if not separator:
            continue
        key = key.strip().lower()
        value = value.strip()
        if key == "source_sha":
            result["source_sha"] = value or None
        elif key == "worktree_dirty":
            result["worktree_dirty"] = value or None
    return result


def load_records(run_dir: Path, paths: list[Path] | None = None) -> dict[str, dict]:
    """Load aggregate JSON records keyed by their scenario label."""
    json_dir = run_dir / "json"
    if paths is None:
        paths = discover_json_files(run_dir)
    records: dict[str, dict] = {}
    for path in paths:
        try:
            text = path.read_text(encoding="utf-8")
        except OSError as exc:
            raise CompareError(f"cannot read {path}: {exc}") from exc
        try:
            record = json.loads(text)
        except json.JSONDecodeError as exc:
            raise CompareError(f"invalid JSON in {path}: {exc}") from exc
        if not isinstance(record, dict):
            raise CompareError(f"{path}: top-level JSON value is not an object")
        label = record.get("scenario")
        scenario = label if isinstance(label, str) and label else path.stem
        if scenario in records:
            raise CompareError(f"duplicate scenario label {scenario!r} in {json_dir}")
        records[scenario] = record
    return records


def recognised_scenarios(records: dict[str, dict]) -> list[str]:
    found = [scenario for scenario in SWEEP_SCENARIOS if scenario in records]
    if SOAK_SCENARIO in records:
        found.append(SOAK_SCENARIO)
    return found


def scenario_set_warning(
    before_records: dict[str, dict], after_records: dict[str, dict]
) -> str | None:
    before = recognised_scenarios(before_records)
    after = recognised_scenarios(after_records)
    if set(before) == set(after):
        return None
    return (
        f"scenario sets differ: before=[{', '.join(before)}] "
        f"after=[{', '.join(after)}]; rows marked NOT RECORDED are absent, not zero"
    )


def record_status(record: dict | None) -> str | None:
    if not isinstance(record, dict):
        return None
    status = record.get("status")
    return status if isinstance(status, str) and status else None


def record_statuses(records: dict[str, dict]) -> dict[str, str | None]:
    return {scenario: record_status(record) for scenario, record in records.items()}


def status_note(before_record: dict | None, after_record: dict | None) -> str:
    notes = []
    for label, record in (("before", before_record), ("after", after_record)):
        status = record_status(record)
        if status and status != "OK":
            notes.append(f"{label} status={status}")
    return "; ".join(notes)


def scenario_sort_key(label: str) -> tuple[int, str]:
    known = {scenario: index for index, scenario in enumerate((*SWEEP_SCENARIOS, SOAK_SCENARIO))}
    return (known.get(label, len(known)), label)



def numeric(value: object) -> int | float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    return None


def lookup(record: dict | None, *keys: str) -> int | float | None:
    if not isinstance(record, dict):
        return None
    current: object = record
    for key in keys:
        if not isinstance(current, dict) or key not in current:
            return None
        current = current[key]
    return numeric(current)


def mixed_metrics(record: dict | None) -> dict[str, int | float | None]:
    if not isinstance(record, dict):
        return {key: None for key, _ in CONCURRENCY_METRICS}
    return {
        "ops_per_sec": lookup(record, "throughput_ops_per_second"),
        "p95_ms": lookup(record, "overall", "p95_ms"),
        "read_p95_ms": lookup(record, "read", "p95_ms"),
        "write_p95_ms": lookup(record, "write", "p95_ms"),
        "error_rate": lookup(record, "error_rate"),
    }


def soak_metrics(record: dict | None) -> dict[str, int | float | None]:
    return {key: lookup(record, *path) for key, path in SOAK_PATHS.items()}


def percentage_change(before: int | float | None, after: int | float | None) -> float | None:
    """Return percent change, or None when the baseline is absent/zero."""
    if before is None or after is None or before == 0:
        return None
    return (after - before) / before * 100.0


def per_minute(value: int | float | None, duration_seconds: int | float | None) -> float | None:
    if value is None or duration_seconds is None or duration_seconds <= 0:
        return None
    return float(value) / float(duration_seconds) * 60.0


def make_change(before: object, after: object) -> dict:
    before_value = numeric(before)
    after_value = numeric(after)
    note = ""
    if before_value is None and after_value is None:
        note = "NOT RECORDED in either run"
    elif before_value is None:
        note = "NOT RECORDED in before"
    elif after_value is None:
        note = "NOT RECORDED in after"
    elif before_value == 0:
        note = "percentage change undefined (before is zero)"
    return {
        "before": before_value,
        "after": after_value,
        "delta_abs": (
            None
            if before_value is None or after_value is None
            else after_value - before_value
        ),
        "delta_pct": percentage_change(before_value, after_value),
        "note": note,
    }


def add_note(change: dict, suffix: str) -> dict:
    if suffix:
        change["note"] = f"{change['note']}; {suffix}" if change["note"] else suffix
    return change


def build_model(before_records: dict[str, dict], after_records: dict[str, dict]) -> dict:
    concurrency: list[dict] = []
    for tier, scenario in zip(SWEEP_TIERS, SWEEP_SCENARIOS):
        before_record = before_records.get(scenario)
        after_record = after_records.get(scenario)
        before_values = mixed_metrics(before_record)
        after_values = mixed_metrics(after_record)
        status_suffix = status_note(before_record, after_record)
        metrics = []
        for key, label in CONCURRENCY_METRICS:
            change = make_change(before_values[key], after_values[key])
            metrics.append(
                {
                    "metric": key,
                    "label": label,
                    "change": add_note(change, status_suffix),
                }
            )
        concurrency.append(
            {
                "scenario": scenario,
                "tier": tier,
                "metrics": metrics,
            }
        )

    before_soak_record = before_records.get(SOAK_SCENARIO)
    after_soak_record = after_records.get(SOAK_SCENARIO)
    before_soak = soak_metrics(before_soak_record)
    after_soak = soak_metrics(after_soak_record)
    status_suffix = status_note(before_soak_record, after_soak_record)
    soak = []
    for key, label in SOAK_METRICS:
        change = make_change(before_soak[key], after_soak[key])
        soak.append(
            {
                "metric": key,
                "label": label,
                "change": add_note(change, status_suffix),
            }
        )

    before_duration = before_soak["duration_seconds"]
    after_duration = after_soak["duration_seconds"]
    durations_differ = (
        before_duration is not None
        and after_duration is not None
        and before_duration != after_duration
    )

    normalised = None
    if durations_differ:
        normalised = []
        for key, label, source_key in NORMALISED_METRICS:
            change = make_change(
                per_minute(before_soak[source_key], before_duration),
                per_minute(after_soak[source_key], after_duration),
            )
            suffix = "normalised per minute (soak durations differ)"
            add_note(change, suffix)
            normalised.append({"metric": key, "label": label, "change": change})

    return {
        "concurrency": concurrency,
        "soak": soak,
        "normalised": normalised,
        "before_duration_seconds": before_duration,
        "after_duration_seconds": after_duration,
        "durations_differ": durations_differ,
        "before_statuses": record_statuses(before_records),
        "after_statuses": record_statuses(after_records),
        "scenario_warning": scenario_set_warning(before_records, after_records),
    }



def display(value: int | float | None) -> str:
    if value is None:
        return "NOT RECORDED"
    if isinstance(value, int):
        return str(value)
    if value.is_integer():
        return str(int(value))
    return f"{value:.10g}"


def md_value(change: dict, key: str) -> str:
    value = change[key]
    if value is None:
        if (
            key == "delta_pct"
            and change["before"] is not None
            and change["after"] is not None
            and change["before"] == 0
        ):
            return "n/a (zero baseline)"
        return "NOT RECORDED"
    return display(value)


def duration_text(seconds: int | float | None) -> str:
    value = numeric(seconds)
    if value is None:
        return "NOT RECORDED"
    return f"{display(value)} s ({float(value) / 60.0:.3f} min)"


def markdown_table(header: list[str], rows: list[list[str]]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def csv_cell(value: int | float | None) -> object:
    return "" if value is None else value


def write_comparison_csv(path: Path, model: dict) -> None:
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(
            ["scenario", "metric", "before", "after", "delta_abs", "delta_pct", "note"]
        )
        for group in model["concurrency"]:
            for row in group["metrics"]:
                change = row["change"]
                writer.writerow(
                    [
                        group["scenario"],
                        row["metric"],
                        csv_cell(change["before"]),
                        csv_cell(change["after"]),
                        csv_cell(change["delta_abs"]),
                        csv_cell(change["delta_pct"]),
                        change["note"],
                    ]
                )
        for row in model["soak"]:
            change = row["change"]
            writer.writerow(
                [
                    SOAK_SCENARIO,
                    row["metric"],
                    csv_cell(change["before"]),
                    csv_cell(change["after"]),
                    csv_cell(change["delta_abs"]),
                    csv_cell(change["delta_pct"]),
                    change["note"],
                ]
            )
        for row in model["normalised"] or []:
            change = row["change"]
            writer.writerow(
                [
                    "soak-normalised-per-minute",
                    row["metric"],
                    csv_cell(change["before"]),
                    csv_cell(change["after"]),
                    csv_cell(change["delta_abs"]),
                    csv_cell(change["delta_pct"]),
                    change["note"],
                ]
            )


def render_markdown(model: dict, source: dict) -> str:
    lines: list[str] = [
        "# Benchmark comparison",
        "",
        f"> **Caveat:** {CAVEAT}",
        "",
    ]
    if model.get("scenario_warning"):
        lines.extend([f"> **WARNING:** {model['scenario_warning']}", ""])
    non_ok = [
        f"{label} {scenario}={status}"
        for label, statuses in (
            ("before", model["before_statuses"]),
            ("after", model["after_statuses"]),
        )
        for scenario, status in statuses.items()
        if status and status != "OK"
    ]
    if non_ok:
        lines.extend(
            [
                "> **WARNING:** non-OK record status: "
                + ", ".join(sorted(non_ok))
                + ". Missing metrics are NOT RECORDED, not zero.",
                "",
            ]
        )
    lines.extend(
        [
            "Source SHAs and dirty flags below are read from each run's",
            "`ENVIRONMENT.txt`; the soak durations are read from each `json/soak.json`.",
            "",
            "## Sources",
            "",
        ]
    )

    source_rows: list[list[str]] = []
    for label in ("before", "after"):
        info = source[label]
        sha = info["source_sha"]
        dirty = info["worktree_dirty"]
        source_rows.append(
            [
                label,
                f"`{info['path']}`",
                f"`{sha}`" if sha else "NOT RECORDED",
                dirty if dirty else "NOT RECORDED",
                info["manifest"],
                duration_text(info["soak_duration_seconds"]),
            ]
        )
    lines.append(
        markdown_table(
            ["run", "directory", "source SHA", "worktree dirty", "manifest", "soak duration"],
            source_rows,
        )
    )

    status_labels = sorted(
        set(model["before_statuses"]) | set(model["after_statuses"]),
        key=scenario_sort_key,
    )
    status_rows = [
        [
            scenario,
            model["before_statuses"].get(scenario) or "NOT RECORDED",
            model["after_statuses"].get(scenario) or "NOT RECORDED",
        ]
        for scenario in status_labels
    ]
    lines.extend(
        [
            "",
            "### Record statuses",
            "",
            "The harness `status` field for every aggregate JSON record. A",
            "`SETUP_FAILED` or other non-OK status is therefore not confused with",
            "an omitted metric.",
            "",
            markdown_table(["scenario", "before status", "after status"], status_rows),
        ]
    )

    lines.extend(
        [
            "",
            "## Mixed concurrency sweep",
            "",
            "Concurrency rows are the harness's `mixed-c1`, `mixed-c8`, `mixed-c32`",
            "and `mixed-c128` aggregates. p95 values are nearest-rank across every",
            "recorded request.",
            "",
        ]
    )
    concurrency_rows: list[list[str]] = []
    for group in model["concurrency"]:
        for row in group["metrics"]:
            change = row["change"]
            concurrency_rows.append(
                [
                    group["scenario"],
                    str(group["tier"]),
                    row["label"],
                    md_value(change, "before"),
                    md_value(change, "after"),
                    md_value(change, "delta_abs"),
                    md_value(change, "delta_pct"),
                    change["note"],
                ]
            )
    lines.append(
        markdown_table(
            [
                "scenario",
                "in-flight",
                "metric",
                "before",
                "after",
                "delta abs",
                "delta %",
                "note",
            ],
            concurrency_rows,
        )
    )

    lines.extend(["", "## Soak", ""])
    soak_rows: list[list[str]] = []
    for row in model["soak"]:
        change = row["change"]
        soak_rows.append(
            [
                row["label"],
                md_value(change, "before"),
                md_value(change, "after"),
                md_value(change, "delta_abs"),
                md_value(change, "delta_pct"),
                change["note"],
            ]
        )
    lines.append(
        markdown_table(
            ["metric", "before", "after", "delta abs", "delta %", "note"],
            soak_rows,
        )
    )

    if model["normalised"] is not None:
        lines.extend(
            [
                "",
                "## Normalised per minute",
                "",
                "The soak durations differ, so duration-sensitive quantities are",
                "also shown per minute. These rows are derived from the recorded",
                "durations, ops and resource deltas; they are not additional",
                "measurements.",
                "",
            ]
        )
        normalised_rows: list[list[str]] = []
        for row in model["normalised"]:
            change = row["change"]
            normalised_rows.append(
                [
                    row["label"],
                    md_value(change, "before"),
                    md_value(change, "after"),
                    md_value(change, "delta_abs"),
                    md_value(change, "delta_pct"),
                    change["note"],
                ]
            )
        lines.append(
            markdown_table(
                ["metric", "before", "after", "delta abs", "delta %", "note"],
                normalised_rows,
            )
        )
    else:
        lines.extend(
            [
                "",
                "_Per-minute normalisation was not applied: the two soak durations",
                "are equal or were not recorded._",
            ]
        )

    lines.extend(
        [
            "",
            "## Reading this comparison",
            "",
            "- `NOT RECORDED` means the harness JSON did not contain that value; it",
            "  is never treated as zero.",
            "- A percentage change with a zero before value is undefined and is",
            "  shown as `n/a (zero baseline)` in Markdown; the CSV cell is empty.",
            "- The sweep changes both in-flight work and the number of live volumes",
            "  (each worker owns one volume). It is not a fixed-dataset comparison.",
            "- These are control-plane results only. No guest I/O or data path is",
            "  measured, and no production SLA is implied.",
            "",
        ]
    )
    return "\n".join(lines)


def build_json_payload(model: dict, source: dict) -> dict:
    return {
        "before": {
            "run_dir": source["before"]["path"],
            "source_sha": source["before"]["source_sha"],
            "worktree_dirty": source["before"]["worktree_dirty"],
            "manifest": source["before"]["manifest"],
            "soak_duration_seconds": model["before_duration_seconds"],
        },
        "after": {
            "run_dir": source["after"]["path"],
            "source_sha": source["after"]["source_sha"],
            "worktree_dirty": source["after"]["worktree_dirty"],
            "manifest": source["after"]["manifest"],
            "soak_duration_seconds": model["after_duration_seconds"],
        },
        "concurrency": [
            {
                "scenario": group["scenario"],
                "concurrency": group["tier"],
                "metrics": {row["metric"]: row["change"] for row in group["metrics"]},
            }
            for group in model["concurrency"]
        ],
        "soak": {row["metric"]: row["change"] for row in model["soak"]},
        "normalised_per_minute": (
            None
            if model["normalised"] is None
            else {row["metric"]: row["change"] for row in model["normalised"]}
        ),
        "record_status": {
            "before": model["before_statuses"],
            "after": model["after_statuses"],
        },
        "scenario_warning": model["scenario_warning"],
        "caveat": CAVEAT,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Compare two benchmark run directories. Each MANIFEST.sha256 is "
            "verified before any report is written."
        )
    )
    parser.add_argument("--before", required=True, help="baseline run directory")
    parser.add_argument("--after", required=True, help="candidate run directory")
    parser.add_argument("--out", required=True, help="output directory")
    parser.add_argument(
        "--json",
        action="store_true",
        help="also print a machine-readable comparison summary to stdout",
    )
    parser.add_argument(
        "--skip-manifest-check",
        action="store_true",
        help="skip MANIFEST.sha256 verification (tests/debugging only)",
    )
    args = parser.parse_args(argv)

    before_dir = Path(args.before)
    after_dir = Path(args.after)
    out_dir = Path(args.out)
    runs = {"before": before_dir, "after": after_dir}

    for label, run_dir in runs.items():
        if not run_dir.is_dir():
            print(f"compare: {label} run directory not found: {run_dir}", file=sys.stderr)
            return 1

    manifest_status = "skipped (--skip-manifest-check)"
    manifest_results: dict[str, ManifestVerification] = {}
    if not args.skip_manifest_check:
        manifest_failed = False
        for label, run_dir in runs.items():
            verification = verify_manifest(run_dir)
            manifest_results[label] = verification
            if verification.failures:
                manifest_failed = True
                print(
                    f"compare: {label} manifest verification failed for {run_dir}:",
                    file=sys.stderr,
                )
                for failure in verification.failures:
                    print(f"  {failure}", file=sys.stderr)
        if manifest_failed:
            print(
                "compare: refusing to write a comparison report from unverified input",
                file=sys.stderr,
            )
            return 1
        manifest_status = "verified"

    try:
        json_files = {
            label: discover_json_files(run_dir) for label, run_dir in runs.items()
        }
    except CompareError as exc:
        print(f"compare: {exc}", file=sys.stderr)
        return 1

    if not args.skip_manifest_check:
        coverage_failed = False
        for label, run_dir in runs.items():
            failures = manifest_covers_json(
                run_dir, json_files[label], manifest_results[label]
            )
            if failures:
                coverage_failed = True
                print(
                    f"compare: {label} manifest does not cover the compared JSON:",
                    file=sys.stderr,
                )
                for failure in failures:
                    print(f"  {failure}", file=sys.stderr)
        if coverage_failed:
            print(
                "compare: refusing to write a comparison report from unverified input",
                file=sys.stderr,
            )
            return 1

    try:
        before_records = load_records(before_dir, json_files["before"])
        after_records = load_records(after_dir, json_files["after"])
    except CompareError as exc:
        print(f"compare: {exc}", file=sys.stderr)
        return 1

    for label, records in (("before", before_records), ("after", after_records)):
        if not recognised_scenarios(records):
            print(
                f"compare: no recognised records in {label} ({runs[label]}): "
                "expected mixed-c1/c8/c32/c128 or soak JSON",
                file=sys.stderr,
            )
            return 1

    model = build_model(before_records, after_records)
    if model["scenario_warning"]:
        print(f"compare: WARNING: {model['scenario_warning']}", file=sys.stderr)
    source = {
        "before": {
            "path": str(before_dir),
            "manifest": manifest_status,
            **parse_environment(before_dir),
            "soak_duration_seconds": model["before_duration_seconds"],
        },
        "after": {
            "path": str(after_dir),
            "manifest": manifest_status,
            **parse_environment(after_dir),
            "soak_duration_seconds": model["after_duration_seconds"],
        },
    }

    try:
        out_dir.mkdir(parents=True, exist_ok=True)
        write_comparison_csv(out_dir / "comparison.csv", model)
        (out_dir / "comparison.md").write_text(
            render_markdown(model, source), encoding="utf-8"
        )
    except OSError as exc:
        print(f"compare: cannot write report to {out_dir}: {exc}", file=sys.stderr)
        return 1

    if args.json:
        print(json.dumps(build_json_payload(model, source), indent=2, sort_keys=True))
    print(
        f"compare: wrote {out_dir / 'comparison.csv'} and {out_dir / 'comparison.md'}",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
