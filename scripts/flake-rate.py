#!/usr/bin/env python3
"""Compute the observed flake rate from `campaign-flake` JSONL samples.

This is an evidence calculator, not an evidence claim. It reads the sample
records the `campaign-flake` workflow writes (one JSON object per line with
`iteration`, `result`, `started`, `finished` and `commit`) and reports:

  * total/pass/fail counts and the exact `failures/total` flake rate;
  * the distinct UTC dates observed and per-date sample/failure counts;
  * the longest run of consecutive calendar dates with samples and no
    failures;
  * duplicate `(commit, iteration)` entries; and
  * whether issue #49's 14-consecutive-night criterion is met **by the
    supplied data only**.

The calculator never infers a night that was not sampled and never counts
multiple samples on one date as multiple nights. Supply the downloaded
historical artifacts, not a single workflow run, to assess the criterion.
See docs/testing/flake-rate.md.
"""

from __future__ import annotations

import argparse
import glob
import json
import sys
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

REQUIRED_FIELDS = ("iteration", "result", "started", "finished", "commit")
KNOWN_RESULTS = {"pass", "fail"}
CRITERION_NIGHTS = 14

EXIT_OK = 0
EXIT_INVALID = 1
EXIT_USAGE = 2


class SampleError(Exception):
    """An input file or sample record is not valid campaign-flake evidence."""


@dataclass(frozen=True)
class Sample:
    iteration: int
    result: str
    started: datetime
    finished: datetime
    commit: str


def parse_utc(value: object, field: str, where: str) -> datetime:
    """Parse an ISO-8601 timestamp and require an explicit UTC offset."""
    if not isinstance(value, str) or not value:
        raise SampleError(f"{where}: {field} must be a non-empty ISO-8601 string")
    text = value[:-1] + "+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError as error:
        raise SampleError(
            f"{where}: {field} is not a valid ISO-8601 timestamp: {value!r} ({error})"
        ) from error
    if parsed.tzinfo is None or parsed.utcoffset() != timedelta(0):
        raise SampleError(
            f"{where}: {field} must carry a UTC offset (Z or +00:00): {value!r}"
        )
    return parsed.astimezone(timezone.utc)


def expand_paths(raw_paths: list[str]) -> list[Path]:
    """Expand shell-style globs here too, for callers that quote them."""
    paths: list[Path] = []
    for raw in raw_paths:
        if any(character in raw for character in "*?["):
            matches = sorted(glob.glob(raw))
            if not matches:
                raise SampleError(f"no files matched pattern: {raw}")
            paths.extend(Path(match) for match in matches)
        else:
            paths.append(Path(raw))
    return paths


def load_samples(paths: list[Path]) -> list[Sample]:
    samples: list[Sample] = []
    for path in paths:
        if not path.is_file():
            raise SampleError(f"{path}: not a readable file")
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise SampleError(f"{path}: cannot read as UTF-8 text: {error}") from error
        for line_number, line in enumerate(text.splitlines(), start=1):
            if not line.strip():
                continue
            where = f"{path}:{line_number}"
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise SampleError(
                    f"{where}: malformed JSON: {error.msg} (column {error.colno})"
                ) from error
            if not isinstance(record, dict):
                raise SampleError(f"{where}: each JSONL line must be a JSON object")
            missing = [field for field in REQUIRED_FIELDS if field not in record]
            if missing:
                raise SampleError(
                    f"{where}: missing required field(s): {', '.join(missing)}"
                )

            iteration = record["iteration"]
            if isinstance(iteration, bool) or not isinstance(iteration, int) or iteration < 1:
                raise SampleError(
                    f"{where}: iteration must be a positive integer, found {iteration!r}"
                )
            result = record["result"]
            if result not in KNOWN_RESULTS:
                expected = "', '".join(sorted(KNOWN_RESULTS))
                raise SampleError(
                    f"{where}: unknown result {result!r}; expected '{expected}'"
                )
            commit = record["commit"]
            if not isinstance(commit, str) or not commit:
                raise SampleError(f"{where}: commit must be a non-empty string")
            started = parse_utc(record["started"], "started", where)
            finished = parse_utc(record["finished"], "finished", where)
            if finished < started:
                raise SampleError(
                    f"{where}: finished {record['finished']!r} is before "
                    f"started {record['started']!r}"
                )
            samples.append(
                Sample(
                    iteration=iteration,
                    result=result,
                    started=started,
                    finished=finished,
                    commit=commit,
                )
            )
    return samples


def longest_green_streak(per_date: dict[str, dict[str, int]]) -> int:
    """Longest run of consecutive calendar dates with samples and no failures."""
    longest = 0
    streak = 0
    previous: date | None = None
    for day_text in sorted(per_date):
        day = date.fromisoformat(day_text)
        if per_date[day_text]["failures"] == 0:
            consecutive = previous is not None and day == previous + timedelta(days=1)
            streak = streak + 1 if consecutive and streak > 0 else 1
        else:
            streak = 0
        longest = max(longest, streak)
        previous = day
    return longest


def analyze(samples: list[Sample], files: list[Path]) -> dict[str, object]:
    total = len(samples)
    failures = sum(1 for sample in samples if sample.result == "fail")
    passes = total - failures
    per_date_counts: dict[str, dict[str, int]] = defaultdict(
        lambda: {"samples": 0, "failures": 0}
    )
    for sample in samples:
        day = sample.started.date().isoformat()
        per_date_counts[day]["samples"] += 1
        if sample.result == "fail":
            per_date_counts[day]["failures"] += 1

    per_date = [
        {
            "date": day,
            "samples": counts["samples"],
            "failures": counts["failures"],
        }
        for day, counts in sorted(per_date_counts.items())
    ]
    dates = [entry["date"] for entry in per_date]

    pair_counts = Counter((sample.commit, sample.iteration) for sample in samples)
    duplicates = [
        {"commit": commit, "iteration": iteration, "count": count}
        for (commit, iteration), count in sorted(pair_counts.items())
        if count > 1
    ]
    green_streak = longest_green_streak(per_date_counts)
    rate = failures / total if total else None

    return {
        "files": [str(path) for path in files],
        "total_samples": total,
        "passes": passes,
        "failures": failures,
        "flake_rate": rate,
        "flake_numerator": failures,
        "flake_denominator": total,
        "distinct_utc_dates": dates,
        "per_date": per_date,
        "longest_green_streak": green_streak,
        "required_green_nights": CRITERION_NIGHTS,
        "criterion_met": green_streak >= CRITERION_NIGHTS,
        "duplicate_commit_iterations": duplicates,
        "date_range": {
            "first": dates[0] if dates else None,
            "last": dates[-1] if dates else None,
        },
        "empty": total == 0,
    }


def render_text(report: dict[str, object]) -> str:
    lines = ["flake-rate report"]
    files = report["files"]
    total = int(report["total_samples"])
    failures = int(report["failures"])
    passes = int(report["passes"])
    lines.append(f"  files: {len(files)}")
    lines.append(f"  samples: {total}")
    lines.append(f"  passes: {passes}")
    lines.append(f"  failures: {failures}")
    if total:
        rate = report["flake_rate"]
        lines.append(
            f"  flake_rate: {float(rate):.6f} "
            f"({failures}/{total})"
        )
    else:
        lines.append("  flake_rate: n/a (0 samples)")

    if report["empty"]:
        lines.append("  no samples found in the supplied JSONL input")
    else:
        date_range = report["date_range"]
        lines.append(
            f"  date range: {date_range['first']} .. {date_range['last']} "
            f"({len(report['distinct_utc_dates'])} distinct UTC dates)"
        )
        lines.append("  per-date samples/failures:")
        for entry in report["per_date"]:
            lines.append(
                f"    {entry['date']}: samples={entry['samples']} "
                f"failures={entry['failures']}"
            )

    lines.append(
        f"  longest consecutive green dates: {report['longest_green_streak']}"
    )
    lines.append(
        f"  required consecutive green dates: {report['required_green_nights']}"
    )
    duplicates = report["duplicate_commit_iterations"]
    if duplicates:
        lines.append("  duplicate (commit, iteration) entries:")
        for entry in duplicates:
            lines.append(
                f"    commit={entry['commit']} iteration={entry['iteration']} "
                f"count={entry['count']}"
            )
    else:
        lines.append("  duplicate (commit, iteration) entries: none")

    lines.append(f"  criterion_met: {str(report['criterion_met']).lower()}")
    lines.append(
        "  note: absent nights are not inferred; the criterion covers only "
        "the supplied samples. One workflow run is not 14 nights."
    )
    return "\n".join(lines)


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Compute an observed flake rate and the 14-consecutive-night "
            "criterion from campaign-flake JSONL samples."
        ),
        epilog=(
            "Exit codes: 0 valid/success, 1 invalid input, 2 usage. "
            "Supply downloaded historical artifacts, not one run, when "
            "assessing the 14-night criterion. See docs/testing/flake-rate.md."
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="emit the report as JSON instead of text",
    )
    parser.add_argument(
        "--allow-empty",
        action="store_true",
        help="treat an input with zero samples as success (still criterion_met=false)",
    )
    parser.add_argument(
        "files",
        nargs="*",
        metavar="JSONL",
        help="campaign-flake iterations.jsonl files (shell globs are expanded)",
    )
    args = parser.parse_args(argv)
    if not args.files:
        parser.error("at least one JSONL file is required")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        paths = expand_paths(args.files)
        samples = load_samples(paths)
    except SampleError as error:
        print(f"flake-rate: ERROR: {error}", file=sys.stderr)
        return EXIT_INVALID

    report = analyze(samples, paths)
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(render_text(report))

    if report["empty"]:
        if args.allow_empty:
            return EXIT_OK
        print(
            "flake-rate: ERROR: no samples found; pass --allow-empty to "
            "accept an empty input explicitly",
            file=sys.stderr,
        )
        return EXIT_INVALID
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
