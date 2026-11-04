"""Tests for scripts/validate-traces.py (committed trace evidence).

The validator is an evidence gate, so its accept/reject boundaries are
themselves tested: a regression that makes it too lenient must fail CI.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "validate-traces.py"
ATTACH = ROOT / "docs" / "operations" / "traces" / "attach.jsonl"


def load_validator():
    spec = importlib.util.spec_from_file_location("validate_traces", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


VALIDATOR = load_validator()


def run(*arguments: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SCRIPT), *arguments],
        capture_output=True,
        text=True,
        check=False,
    )


def write_trace(directory: str, lines: list[dict], name: str = "trace.jsonl") -> Path:
    path = Path(directory) / name
    path.write_text(
        "".join(json.dumps(line, separators=(",", ":")) + "\n" for line in lines),
        encoding="utf-8",
    )
    return path


def attach_events() -> list[dict]:
    return [
        json.loads(line)
        for line in ATTACH.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def operation(name: str, operation_id: str, *, kind: str | None = None) -> dict:
    return {
        "timestamp": "2026-11-04T14:25:37Z",
        "level": "INFO",
        "fields": {"message": "operation completed", "kind": kind or name},
        "span": {
            "name": "operation",
            "kind": kind or name,
            "operation_id": operation_id,
        },
    }


class Rfc3339Tests(unittest.TestCase):
    def test_accepts_z_fraction_and_numeric_offsets(self) -> None:
        for value in (
            "2026-11-04T14:25:37Z",
            "2026-11-04T14:25:37z",
            "2026-11-04T14:25:37.123456Z",
            "2026-11-04T14:25:37+02:00",
            "2026-11-04T14:25:37-07:30",
        ):
            with self.subTest(value=value):
                self.assertTrue(VALIDATOR.is_rfc3339(value))

    def test_rejects_malformed_values(self) -> None:
        for value in (
            "2026-11-04 14:25:37Z",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-11-04T24:00:00Z",
            "2026-11-04T14:25:37",
            "2026-11-04T14:25:37+0200",
            "2026-11-04T14:25:37.Z",
            "2026-11-04T14:25:37+24:00",
            "2026-11-04T14:25:37Zextra",
            "not-a-timestamp",
        ):
            with self.subTest(value=value):
                self.assertFalse(VALIDATOR.is_rfc3339(value))

    def test_rejects_non_ascii_digits(self) -> None:
        # str.isdigit() accepts Unicode decimal digits; a committed trace may
        # not, even though int() would happily parse some of them.
        for value in (
            "\u0662\u0660\u0662\u0666-11-04T14:25:37Z",
            "2026-11-04T\u0661\u0664:25:37Z",
            "2026-11-04T14:25:37.\u0661\u0662\u0663Z",
            "2026-11-04T14:25:37+\u0660\u0662:00",
        ):
            with self.subTest(value=value):
                self.assertFalse(VALIDATOR.is_rfc3339(value))

    def test_rfc3339_helpers(self) -> None:
        self.assertTrue(VALIDATOR.ascii_digits("2026"))
        self.assertFalse(VALIDATOR.ascii_digits(""))
        self.assertFalse(VALIDATOR.ascii_digits("\u0662\u0660\u0662\u0666"))
        self.assertFalse(VALIDATOR.ascii_digits("20a6"))


class ValidatorCliTests(unittest.TestCase):
    def test_committed_attach_trace_passes(self) -> None:
        result = run(str(ATTACH))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_empty_file_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "empty.jsonl"
            path.write_text("\n\n", encoding="utf-8")
            result = run(str(path))
        self.assertEqual(result.returncode, 1)
        self.assertIn("no events", result.stderr)

    def test_secret_like_field_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event = operation("create_volume", "op-1")
            event["fields"]["api_token"] = "redacted"
            result = run(str(write_trace(directory, [event])))
        self.assertEqual(result.returncode, 1)
        self.assertIn("secret-like field", result.stderr)

    def test_test_admin_token_value_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event = operation("create_volume", "op-1")
            event["fields"]["message"] = "using trace-admin-token"
            result = run(str(write_trace(directory, [event])))
        self.assertEqual(result.returncode, 1)
        self.assertIn("test admin token", result.stderr)

    def test_unknown_span_name_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event = operation("create_volume", "op-1")
            event["span"]["name"] = "made_up_span"
            result = run(str(write_trace(directory, [event])))
        self.assertEqual(result.returncode, 1)
        self.assertIn("unknown span name", result.stderr)

    def test_provider_call_must_include_its_operation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event = {
                "timestamp": "2026-11-04T14:25:37Z",
                "level": "INFO",
                "fields": {"message": "provider call completed"},
                "span": {
                    "name": "provider_call",
                    "operation_id": "op-1",
                },
            }
            result = run(str(write_trace(directory, [event])))
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not include the enclosing operation", result.stderr)

    def test_provider_call_id_must_match_the_operation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            event = operation("create_volume", "op-1")
            event["spans"] = [
                {"name": "operation", "kind": "create_volume", "operation_id": "op-1"},
                {
                    "name": "provider_call",
                    "kind": "create_volume",
                    "operation_id": "op-2",
                },
            ]
            result = run(str(write_trace(directory, [event])))
        self.assertEqual(result.returncode, 1)
        self.assertIn("does not match enclosing operation", result.stderr)

    def test_attach_file_requires_both_audit_correlations(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            events = attach_events()
            # Every span-only line validates on its own, but the audit events
            # are what prove the operation ids reached the audit route.
            without_audit = [e for e in events if e["fields"].get("kind") != "audit"]
            result = run(str(write_trace(directory, without_audit, "attach.jsonl")))
        self.assertEqual(result.returncode, 1)
        self.assertIn("no create audit event", result.stderr)

    def test_attach_correlation_passes_on_the_committed_events(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            result = run(str(write_trace(directory, attach_events(), "attach.jsonl")))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_audit_event_with_an_empty_operation_id_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            events = attach_events()
            audit = next(e for e in events if e["fields"].get("kind") == "audit")
            audit["fields"]["operation_id"] = ""
            result = run(str(write_trace(directory, events)))
        self.assertEqual(result.returncode, 1)
        self.assertIn("non-empty operation_id", result.stderr)

    def test_deeply_nested_json_fails_without_a_traceback(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            nested = "[" * 5000 + "]" * 5000
            line = (
                '{"timestamp":"2026-11-04T14:25:37Z","level":"INFO",'
                '"fields":{"message":"x"},"extra":' + nested + "}\n"
            )
            path = Path(directory) / "trace.jsonl"
            path.write_text(line, encoding="utf-8")
            result = run(str(path))
        self.assertEqual(result.returncode, 1)
        self.assertNotIn("Traceback", result.stderr)
        self.assertIn("nesting", result.stderr)

    def test_missing_file_is_a_validation_failure(self) -> None:
        result = run("/nonexistent/trace.jsonl")
        self.assertEqual(result.returncode, 1)
        self.assertIn("cannot read", result.stderr)

    def test_no_arguments_is_a_usage_error(self) -> None:
        result = run()
        self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
