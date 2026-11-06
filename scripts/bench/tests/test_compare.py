"""Tests for scripts/bench/compare.py (benchmark run comparison)."""

from __future__ import annotations

import csv
import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = ROOT / "scripts" / "bench" / "compare.py"
SWEEP_TIERS = (1, 8, 32, 128)


def load_compare():
    spec = importlib.util.spec_from_file_location("bench_compare", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


COMPARE = load_compare()


def run(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), *arguments],
        capture_output=True,
        text=True,
        check=False,
    )


def write_manifest(run_dir: Path) -> None:
    lines = []
    for path in sorted(run_dir.rglob("*")):
        if path.is_file() and path.name != "MANIFEST.sha256":
            relative = "./" + path.relative_to(run_dir).as_posix()
            digest = hashlib.sha256(path.read_bytes()).hexdigest()
            lines.append(f"{digest}  {relative}")
    (run_dir / "MANIFEST.sha256").write_text("\n".join(lines) + "\n", encoding="utf-8")


def mixed_record(
    tier: int,
    *,
    ops_per_sec: float,
    p95_ms: float,
    read_p95_ms: float,
    write_p95_ms: float,
    error_rate: float = 0.0,
) -> dict:
    """A small record with the real harness's mixed-* JSON shape."""
    return {
        "scenario": f"mixed-c{tier}",
        "status": "OK",
        "concurrency": tier,
        "duration_seconds": 30.0,
        "ops": 10000,
        "errors": 0,
        "error_rate": error_rate,
        "network_errors": 0,
        "throughput_ops_per_second": ops_per_sec,
        "successful_ops_per_second": ops_per_sec,
        "read_ratio_target": 0.7,
        "read_ratio_actual": 0.7,
        "admission_cap": 256,
        "queue_timeout_seconds": 5.0,
        "rate_limit_per_second": 0,
        "overall": {"count": 10000, "p50_ms": p95_ms / 2.0, "p95_ms": p95_ms},
        "accepted": {"count": 10000, "p95_ms": p95_ms},
        "read": {"count": 7000, "p95_ms": read_p95_ms},
        "write": {"count": 3000, "p95_ms": write_p95_ms},
        "status_counts": {"200": 10000},
        "resources": {
            "samples": 29,
            "rss_kib": {"first": 1000.0, "last": 1100.0, "delta": 100.0},
            "journal_bytes": {"first": 1000.0, "last": 2000.0, "delta": 1000.0},
            "state_bytes": {"first": 500.0, "last": 600.0, "delta": 100.0},
        },
        "source_sha": "fixture-sha",
        "worktree_dirty": False,
    }


def soak_record(
    *,
    duration_seconds: float,
    ops: int,
    rss_first: float = 1000.0,
    rss_last: float = 1500.0,
    journal_first: float = 1000.0,
    journal_last: float = 4000.0,
    state_first: float = 500.0,
    state_last: float = 900.0,
) -> dict:
    """A small record with the real harness's soak JSON shape."""
    return {
        "scenario": "soak",
        "status": "OK",
        "duration_seconds": duration_seconds,
        "ops": ops,
        "errors": 0,
        "error_rate": 0.0,
        "workers": 4,
        "throughput_ops_per_second": ops / duration_seconds if duration_seconds else None,
        "successful_ops_per_second": ops / duration_seconds if duration_seconds else None,
        "overall": {"count": ops, "p95_ms": 10.0},
        "read": {"count": ops // 2, "p95_ms": 5.0},
        "write": {"count": ops // 2, "p95_ms": 20.0},
        "resources": {
            "samples": 10,
            "rss_kib": {
                "first": rss_first,
                "last": rss_last,
                "delta": rss_last - rss_first,
            },
            "journal_bytes": {
                "first": journal_first,
                "last": journal_last,
                "delta": journal_last - journal_first,
            },
            "state_bytes": {
                "first": state_first,
                "last": state_last,
                "delta": state_last - state_first,
            },
        },
        "restarts": [],
        "cleanup_failures": [],
        "source_sha": "fixture-sha",
        "worktree_dirty": False,
    }


def write_run_dir(
    base: Path,
    name: str,
    *,
    sha: str,
    dirty: str = "no",
    mixed_overrides: dict[int, dict] | None = None,
    soak: dict | None = None,
    tiers: tuple[int, ...] = SWEEP_TIERS,
    include_soak: bool = True,
    additional_records: dict[str, dict] | None = None,
) -> Path:
    run_dir = base / name
    (run_dir / "json").mkdir(parents=True)
    (run_dir / "records").mkdir()
    overrides = mixed_overrides or {}
    for tier in tiers:
        values = {
            "ops_per_sec": 1000.0 + tier,
            "p95_ms": 1.0 + tier / 10.0,
            "read_p95_ms": 0.8 + tier / 10.0,
            "write_p95_ms": 2.0 + tier / 10.0,
            "error_rate": 0.0,
        }
        values.update(overrides.get(tier, {}))
        record = mixed_record(tier, **values)
        (run_dir / "json" / f"mixed-c{tier}.json").write_text(
            json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    if include_soak:
        if soak is None:
            soak = soak_record(duration_seconds=600.0, ops=6000)
        (run_dir / "json" / "soak.json").write_text(
            json.dumps(soak, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    for scenario, record in (additional_records or {}).items():
        (run_dir / "json" / f"{scenario}.json").write_text(
            json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    (run_dir / "ENVIRONMENT.txt").write_text(
        "Volvisor benchmark environment\n"
        "==============================\n"
        f"source_sha:      {sha}\n"
        f"worktree_dirty:  {dirty}\n",
        encoding="utf-8",
    )
    (run_dir / "records" / "lab.env").write_text(
        "port=1234 vg=vgb-test\n", encoding="utf-8"
    )
    write_manifest(run_dir)
    return run_dir


class ManifestTests(unittest.TestCase):
    def test_correct_manifest_is_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            run_dir = write_run_dir(Path(directory), "run", sha="a" * 40)
            verification = COMPARE.verify_manifest(run_dir)
            self.assertEqual(verification.failures, [])
            self.assertIn("json/mixed-c1.json", verification.verified_paths)
            self.assertIn("json/soak.json", verification.verified_paths)

    def test_tampered_file_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(base, "before", sha="a" * 40)
            after = write_run_dir(base, "after", sha="b" * 40)
            target = after / "json" / "mixed-c1.json"
            target.write_text(target.read_text(encoding="utf-8") + "\n", encoding="utf-8")

            verification = COMPARE.verify_manifest(after)
            self.assertTrue(verification.failures)
            self.assertTrue(
                any("FAILED" in failure for failure in verification.failures)
            )
            self.assertTrue(
                any("mixed-c1.json" in failure for failure in verification.failures)
            )

            out_dir = base / "out"
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(out_dir),
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("manifest verification failed", result.stderr)
            self.assertFalse((out_dir / "comparison.md").exists())
            self.assertFalse((out_dir / "comparison.csv").exists())

    def test_json_not_covered_by_manifest_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(base, "before", sha="a" * 40)
            after = write_run_dir(base, "after", sha="b" * 40)

            # A partial manifest that verifies ENVIRONMENT.txt but says
            # nothing about the JSON files the tool is about to read.
            environment = after / "ENVIRONMENT.txt"
            digest = hashlib.sha256(environment.read_bytes()).hexdigest()
            (after / "MANIFEST.sha256").write_text(
                f"{digest}  ./ENVIRONMENT.txt\n", encoding="utf-8"
            )
            target = after / "json" / "mixed-c1.json"
            target.write_text(target.read_text(encoding="utf-8") + "\n", encoding="utf-8")

            verification = COMPARE.verify_manifest(after)
            self.assertEqual(verification.failures, [])

            out_dir = base / "out"
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(out_dir),
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("does not cover the compared JSON", result.stderr)
            self.assertIn("not listed and verified", result.stderr)
            self.assertIn("lists no JSON file", result.stderr)
            self.assertFalse((out_dir / "comparison.md").exists())
            self.assertFalse((out_dir / "comparison.csv").exists())

    def test_nul_byte_in_manifest_path_is_rejected_without_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            run_dir = write_run_dir(Path(directory), "run", sha="a" * 40)
            environment = run_dir / "ENVIRONMENT.txt"
            digest = hashlib.sha256(environment.read_bytes()).hexdigest()
            (run_dir / "MANIFEST.sha256").write_text(
                f"{digest}  ./ENVIRONMENT.txt\n"
                f"{'a' * 64}  ./json/bad\x00name.json\n",
                encoding="utf-8",
            )

            verification = COMPARE.verify_manifest(run_dir)
            self.assertTrue(verification.failures)
            self.assertTrue(
                any("NUL byte" in failure for failure in verification.failures)
            )


class ParsingTests(unittest.TestCase):
    def test_reads_real_shaped_records_and_environment(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(
                base,
                "before",
                sha="a" * 40,
                dirty="no",
                mixed_overrides={128: {"ops_per_sec": 241.716, "p95_ms": 1251.564}},
            )
            after = write_run_dir(
                base,
                "after",
                sha="b" * 40,
                dirty="yes",
                mixed_overrides={128: {"ops_per_sec": 300.0, "p95_ms": 900.0}},
            )
            out_dir = base / "out"
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(out_dir),
                "--json",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(result.stdout)

            self.assertEqual(payload["before"]["source_sha"], "a" * 40)
            self.assertEqual(payload["after"]["source_sha"], "b" * 40)
            self.assertEqual(payload["before"]["worktree_dirty"], "no")
            self.assertEqual(payload["after"]["worktree_dirty"], "yes")
            self.assertEqual(payload["before"]["manifest"], "verified")

            tiers = {row["concurrency"]: row for row in payload["concurrency"]}
            self.assertEqual(set(tiers), set(SWEEP_TIERS))
            c128 = tiers[128]["metrics"]
            self.assertEqual(c128["ops_per_sec"]["before"], 241.716)
            self.assertEqual(c128["ops_per_sec"]["after"], 300.0)
            self.assertEqual(c128["p95_ms"]["before"], 1251.564)
            self.assertIn("soak", payload)

    def test_no_recognised_records_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(base, "before", sha="a" * 40)
            after = write_run_dir(
                base,
                "after",
                sha="b" * 40,
                tiers=(),
                include_soak=False,
                additional_records={
                    "micro": {
                        "scenario": "micro",
                        "status": "OK",
                        "ops": 1,
                    }
                },
            )
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(base / "out"),
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("no recognised records", result.stderr)

    def test_mismatched_scenario_sets_warn_but_still_exit_zero(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(base, "before", sha="a" * 40)
            after = write_run_dir(
                base,
                "after",
                sha="b" * 40,
                tiers=(1,),
                include_soak=False,
            )
            out_dir = base / "out"
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(out_dir),
                "--json",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("scenario sets differ", result.stderr)
            self.assertIn("absent, not zero", result.stderr)

            payload = json.loads(result.stdout)
            self.assertIn("scenario sets differ", payload["scenario_warning"])
            markdown = (out_dir / "comparison.md").read_text(encoding="utf-8")
            self.assertIn("**WARNING:** scenario sets differ", markdown)
            self.assertIn("rows marked NOT RECORDED are absent, not zero", markdown)

    def test_record_status_is_surfaced(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(base, "before", sha="a" * 40)
            after = write_run_dir(
                base,
                "after",
                sha="b" * 40,
                soak={
                    "scenario": "soak",
                    "status": "SETUP_FAILED",
                    "setup_failures": ["soak-v0: precreate 503"],
                    "source_sha": "b" * 40,
                    "worktree_dirty": False,
                },
            )
            out_dir = base / "out"
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(out_dir),
                "--json",
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            payload = json.loads(result.stdout)
            self.assertEqual(payload["record_status"]["after"]["soak"], "SETUP_FAILED")

            markdown = (out_dir / "comparison.md").read_text(encoding="utf-8")
            self.assertIn("### Record statuses", markdown)
            status_line = next(
                line for line in markdown.splitlines() if line.startswith("| soak |")
            )
            self.assertIn("SETUP_FAILED", status_line)
            self.assertIn("after status=SETUP_FAILED", markdown)


class ChangeTests(unittest.TestCase):
    def test_percentage_change(self) -> None:
        self.assertAlmostEqual(COMPARE.percentage_change(100, 125), 25.0)
        self.assertAlmostEqual(COMPARE.percentage_change(125, 100), -20.0)
        self.assertAlmostEqual(COMPARE.percentage_change(2.0, 2.5), 25.0)

    def test_zero_baseline_does_not_divide_by_zero(self) -> None:
        self.assertIsNone(COMPARE.percentage_change(0, 10))
        change = COMPARE.make_change(0, 10)
        self.assertEqual(change["before"], 0)
        self.assertEqual(change["after"], 10)
        self.assertEqual(change["delta_abs"], 10)
        self.assertIsNone(change["delta_pct"])
        self.assertIn("before is zero", change["note"])

    def test_missing_baseline_has_no_percentage(self) -> None:
        self.assertIsNone(COMPARE.percentage_change(None, 10))
        change = COMPARE.make_change(None, 10)
        self.assertIsNone(change["delta_abs"])
        self.assertIsNone(change["delta_pct"])
        self.assertIn("NOT RECORDED", change["note"])


class DurationNormalisationTests(unittest.TestCase):
    def _compare(
        self, base: Path, before_duration: float, after_duration: float
    ) -> tuple[dict, str]:
        before = write_run_dir(
            base,
            "before",
            sha="a" * 40,
            soak=soak_record(duration_seconds=before_duration, ops=600),
        )
        after = write_run_dir(
            base,
            "after",
            sha="b" * 40,
            soak=soak_record(duration_seconds=after_duration, ops=2400),
        )
        out_dir = base / "out"
        result = run(
            "--before",
            str(before),
            "--after",
            str(after),
            "--out",
            str(out_dir),
            "--json",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        markdown = (out_dir / "comparison.md").read_text(encoding="utf-8")
        return json.loads(result.stdout), markdown

    def test_not_applied_when_durations_are_equal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            payload, markdown = self._compare(Path(directory), 60.0, 60.0)
        self.assertIsNone(payload["normalised_per_minute"])
        self.assertNotIn("## Normalised per minute", markdown)

    def test_applied_when_durations_differ(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            payload, markdown = self._compare(Path(directory), 60.0, 120.0)
        self.assertIn("## Normalised per minute", markdown)
        normalised = payload["normalised_per_minute"]
        self.assertIsNotNone(normalised)
        self.assertEqual(normalised["ops_per_minute"]["before"], 600.0)
        self.assertEqual(normalised["ops_per_minute"]["after"], 1200.0)
        self.assertEqual(normalised["ops_per_minute"]["delta_abs"], 600.0)
        self.assertEqual(normalised["ops_per_minute"]["delta_pct"], 100.0)


class MissingMetricTests(unittest.TestCase):
    def test_missing_metric_is_not_recorded_not_zero(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            before = write_run_dir(base, "before", sha="a" * 40)
            after = write_run_dir(base, "after", sha="b" * 40)

            record_path = after / "json" / "mixed-c1.json"
            record = json.loads(record_path.read_text(encoding="utf-8"))
            del record["read"]
            record_path.write_text(
                json.dumps(record, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
            write_manifest(after)

            out_dir = base / "out"
            result = run(
                "--before",
                str(before),
                "--after",
                str(after),
                "--out",
                str(out_dir),
            )
            self.assertEqual(result.returncode, 0, result.stderr)

            with (out_dir / "comparison.csv").open(
                newline="", encoding="utf-8"
            ) as handle:
                rows = list(csv.DictReader(handle))
            row = next(
                item
                for item in rows
                if item["scenario"] == "mixed-c1"
                and item["metric"] == "read_p95_ms"
            )
            self.assertNotEqual(row["before"], "")
            self.assertEqual(row["after"], "")
            self.assertEqual(row["delta_abs"], "")
            self.assertEqual(row["delta_pct"], "")
            self.assertIn("not recorded", row["note"].lower())

            markdown = (out_dir / "comparison.md").read_text(encoding="utf-8")
            matching = [
                line for line in markdown.splitlines() if "read p95 ms" in line
            ]
            self.assertTrue(matching)
            self.assertTrue(any("NOT RECORDED" in line for line in matching))


if __name__ == "__main__":
    unittest.main()
