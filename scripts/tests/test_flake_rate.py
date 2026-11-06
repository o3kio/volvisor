"""Tests for scripts/flake-rate.py (issue #49 evidence calculator)."""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "flake-rate.py"
FIXTURES = Path(__file__).resolve().parent / "fixtures"


def run(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), *arguments],
        capture_output=True,
        text=True,
        check=False,
    )


class FlakeRateReportTests(unittest.TestCase):
    def test_three_green_nights_report_a_zero_rate_but_no_criterion(self) -> None:
        result = run(str(FIXTURES / "all-green-3-nights.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("samples: 3", result.stdout)
        self.assertIn("passes: 3", result.stdout)
        self.assertIn("failures: 0", result.stdout)
        self.assertIn("flake_rate: 0.000000 (0/3)", result.stdout)
        self.assertIn("(3 distinct UTC dates)", result.stdout)
        self.assertIn("longest consecutive green dates: 3", result.stdout)
        self.assertIn("criterion_met: false", result.stdout)

    def test_fourteen_consecutive_green_dates_meet_the_criterion(self) -> None:
        result = run(str(FIXTURES / "fourteen-green-nights.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("samples: 14", result.stdout)
        self.assertIn("longest consecutive green dates: 14", result.stdout)
        self.assertIn("required consecutive green dates: 14", result.stdout)
        self.assertIn("criterion_met: true", result.stdout)

    def test_a_failure_breaks_the_consecutive_run(self) -> None:
        result = run(str(FIXTURES / "failure-breaks-run.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("samples: 16", result.stdout)
        self.assertIn("failures: 1", result.stdout)
        self.assertIn("flake_rate: 0.062500 (1/16)", result.stdout)
        self.assertIn("2026-10-08: samples=1 failures=1", result.stdout)
        # Seven green dates before the failed date and eight after it; the
        # longest run is the later one.
        self.assertIn("longest consecutive green dates: 8", result.stdout)
        self.assertIn("criterion_met: false", result.stdout)

    def test_multiple_samples_on_one_date_do_not_inflate_nights(self) -> None:
        result = run(str(FIXTURES / "multiple-samples-same-date.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("samples: 9", result.stdout)
        self.assertIn("passes: 9", result.stdout)
        self.assertIn("(3 distinct UTC dates)", result.stdout)
        self.assertIn("longest consecutive green dates: 3", result.stdout)
        self.assertIn("criterion_met: false", result.stdout)

    def test_duplicate_commit_iteration_pairs_are_reported(self) -> None:
        result = run("--json", str(FIXTURES / "duplicates.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(
            report["duplicate_commit_iterations"],
            [{"commit": "dup-commit", "iteration": 1, "count": 2}],
        )
        self.assertIn("criterion_met", report)
        self.assertFalse(report["criterion_met"])

    def test_malformed_json_is_a_clear_invalid_input(self) -> None:
        result = run(str(FIXTURES / "malformed.jsonl"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("malformed JSON", result.stderr)

    def test_unknown_result_is_a_clear_invalid_input(self) -> None:
        result = run(str(FIXTURES / "unknown-result.jsonl"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("unknown result", result.stderr)
        self.assertIn("flaky", result.stderr)

    def test_missing_field_is_a_clear_invalid_input(self) -> None:
        result = run(str(FIXTURES / "missing-field.jsonl"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing required field(s): commit", result.stderr)

    def test_non_utc_timestamp_is_rejected(self) -> None:
        result = run(str(FIXTURES / "non-utc-timestamp.jsonl"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("UTC offset", result.stderr)

    def test_empty_input_is_reported_and_fails_without_allow_empty(self) -> None:
        result = run(str(FIXTURES / "empty.jsonl"))
        self.assertEqual(result.returncode, 1)
        self.assertIn("no samples found", result.stdout)
        self.assertIn("--allow-empty", result.stderr)
        self.assertIn("criterion_met: false", result.stdout)

    def test_allow_empty_is_an_explicit_success_but_not_a_criterion(self) -> None:
        result = run("--allow-empty", str(FIXTURES / "empty.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no samples found", result.stdout)
        self.assertIn("criterion_met: false", result.stdout)

    def test_json_report_shape(self) -> None:
        result = run("--json", str(FIXTURES / "fourteen-green-nights.jsonl"))
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(
            set(report),
            {
                "files",
                "total_samples",
                "passes",
                "failures",
                "flake_rate",
                "flake_numerator",
                "flake_denominator",
                "distinct_utc_dates",
                "per_date",
                "longest_green_streak",
                "required_green_nights",
                "criterion_met",
                "duplicate_commit_iterations",
                "date_range",
                "empty",
            },
        )
        self.assertEqual(report["total_samples"], 14)
        self.assertEqual(report["passes"], 14)
        self.assertEqual(report["failures"], 0)
        self.assertEqual(report["flake_rate"], 0.0)
        self.assertEqual(report["flake_numerator"], 0)
        self.assertEqual(report["flake_denominator"], 14)
        self.assertEqual(len(report["distinct_utc_dates"]), 14)
        self.assertEqual(len(report["per_date"]), 14)
        self.assertEqual(report["longest_green_streak"], 14)
        self.assertEqual(report["required_green_nights"], 14)
        self.assertTrue(report["criterion_met"])
        self.assertEqual(report["duplicate_commit_iterations"], [])
        self.assertEqual(
            report["date_range"],
            {"first": "2026-10-01", "last": "2026-10-14"},
        )
        self.assertFalse(report["empty"])
        self.assertEqual(report["per_date"][0], {
            "date": "2026-10-01",
            "samples": 1,
            "failures": 0,
        })

    def test_help_works(self) -> None:
        result = run("--help")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("campaign-flake", result.stdout)
        self.assertIn("Exit codes", result.stdout)

    def test_no_files_is_a_usage_error(self) -> None:
        result = run()
        self.assertEqual(result.returncode, 2)
        self.assertIn("at least one JSONL file is required", result.stderr)


if __name__ == "__main__":
    unittest.main()
