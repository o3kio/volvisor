#!/usr/bin/env python3
"""Validate committed Volvisor JSON trace evidence.

Each non-empty line must be one JSON object with an RFC 3339 ``timestamp``,
``level`` and ``fields.message``. Span objects must use a bounded known name,
``operation``/``provider_call`` spans must carry a non-empty ``operation_id``,
and a ``provider_call`` must be nested under an ``operation`` with the same id.
Secret-like fields and the test admin token value are rejected.

For ``attach.jsonl`` the create and attach operation ids must both appear and
both audit routes must carry the matching id. If a ``migration.jsonl`` is added
later it is validated with the same generic checks; this script never requires
it to exist.

Usage:
    python3 scripts/validate-traces.py docs/operations/traces/attach.jsonl
    python3 scripts/validate-traces.py docs/operations/traces/*.jsonl

Exit status: 0 valid, 1 invalid, 2 usage error. Python 3 stdlib only.
"""

from __future__ import annotations

import argparse
import json
import sys
from datetime import date
from pathlib import Path
from typing import Any

TEST_ADMIN_TOKEN = "trace-admin-token"
KNOWN_SPAN_NAMES = frozenset(
    {
        "operation",
        "provider_call",
        "peer_call",
        "witness_inspect",
        "vmm_state",
        "vmm_call",
        "peer_grant",
    }
)
OPERATION_ID_SPANS = frozenset({"operation", "provider_call"})
SECRET_KEY_MARKERS = ("authorization", "token", "secret", "password")


class InvalidTrace(Exception):
    """A trace file violated the committed evidence format."""


class TraceFacts:
    def __init__(self) -> None:
        self.operation_ids: dict[str, set[str]] = {}
        self.audit_events: list[tuple[str, str]] = []


def fail(path: Path, line: int, message: str) -> None:
    raise InvalidTrace(f"{path}:{line}: {message}")


def ascii_digits(value: str) -> bool:
    """True only for non-empty ASCII decimal digits.

    `str.isdigit()` also accepts non-ASCII digits such as U+0660, which a
    committed trace must never use in a timestamp.
    """
    return bool(value) and value.isascii() and value.isdigit()


def is_rfc3339(value: str) -> bool:
    """Full-date 'T' full-time with a 'Z' or numeric offset."""
    if len(value) < 20:
        return False
    if value[4] != "-" or value[7] != "-":
        return False
    if value[10] not in "Tt" or value[13] != ":" or value[16] != ":":
        return False
    for start, end in ((0, 4), (5, 7), (8, 10), (11, 13), (14, 16), (17, 19)):
        if not ascii_digits(value[start:end]):
            return False
    try:
        year = int(value[0:4])
        month = int(value[5:7])
        day = int(value[8:10])
        date(year, month, day)
    except ValueError:
        return False
    hour = int(value[11:13])
    minute = int(value[14:16])
    second = int(value[17:19])
    if hour > 23 or minute > 59 or second > 60:
        return False

    index = 19
    if value[index] == ".":
        index += 1
        start = index
        while index < len(value) and value[index] in "0123456789":
            index += 1
        if index == start:
            return False
    if index >= len(value):
        return False

    zone = value[index]
    if zone in "Zz":
        return index + 1 == len(value)
    if zone in "+-":
        if index + 6 != len(value) or value[index + 3] != ":":
            return False
        offset_hour = value[index + 1 : index + 3]
        offset_minute = value[index + 4 : index + 6]
        if not ascii_digits(offset_hour) or not ascii_digits(offset_minute):
            return False
        return int(offset_hour) <= 23 and int(offset_minute) <= 59
    return False


def check_no_secret_fields(path: Path, line: int, value: Any) -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            lowered = key.lower()
            if any(marker in lowered for marker in SECRET_KEY_MARKERS):
                fail(path, line, f"secret-like field present: {key}")
            check_no_secret_fields(path, line, child)
    elif isinstance(value, list):
        for child in value:
            check_no_secret_fields(path, line, child)


def validate_span(
    path: Path,
    line: int,
    span: dict[str, Any],
    facts: TraceFacts,
    require_operation_id: bool,
) -> None:
    name = span.get("name")
    if not isinstance(name, str) or not name:
        fail(path, line, "span is missing a non-empty string name")
    if name not in KNOWN_SPAN_NAMES:
        fail(path, line, f"unknown span name: {name}")

    if name in OPERATION_ID_SPANS or require_operation_id:
        operation_id = span.get("operation_id")
        if not isinstance(operation_id, str) or not operation_id.strip():
            fail(path, line, f"{name} span is missing a non-empty operation_id")
        if name == "operation":
            kind = span.get("kind")
            if not isinstance(kind, str) or not kind:
                fail(path, line, "operation span is missing a kind")
            facts.operation_ids.setdefault(kind, set()).add(operation_id)


def validate_line(
    path: Path,
    line: int,
    raw: str,
    facts: TraceFacts,
    require_operation_id: bool,
) -> None:
    if TEST_ADMIN_TOKEN in raw:
        fail(path, line, "contains the test admin token value")
    try:
        event = json.loads(raw)
    except json.JSONDecodeError as error:
        fail(path, line, f"not one JSON object: {error}")
    except RecursionError:
        fail(path, line, "not one JSON object: nesting is too deep")
    if not isinstance(event, dict):
        fail(path, line, "JSON value is not an object")
    try:
        check_no_secret_fields(path, line, event)
    except RecursionError:
        fail(path, line, "JSON nesting is too deep")

    timestamp = event.get("timestamp")
    if not isinstance(timestamp, str):
        fail(path, line, "missing string field timestamp")
    if not is_rfc3339(timestamp):
        fail(path, line, f"timestamp is not RFC 3339: {timestamp}")

    level = event.get("level")
    if not isinstance(level, str) or not level:
        fail(path, line, "missing non-empty string field level")

    fields = event.get("fields")
    if not isinstance(fields, dict):
        fail(path, line, "missing object field fields")
    message = fields.get("message")
    if not isinstance(message, str) or not message:
        fail(path, line, "missing non-empty string field fields.message")

    spans: list[dict[str, Any]] = []
    if "span" in event:
        span = event["span"]
        if not isinstance(span, dict):
            fail(path, line, "span is not an object")
        spans.append(span)
    if "spans" in event:
        entries = event["spans"]
        if not isinstance(entries, list):
            fail(path, line, "spans is not an array")
        for span in entries:
            if not isinstance(span, dict):
                fail(path, line, "spans[] entry is not an object")
            spans.append(span)

    for span in spans:
        validate_span(path, line, span, facts, require_operation_id)

    provider_span = next(
        (span for span in spans if span.get("name") == "provider_call"), None
    )
    if provider_span is not None:
        provider_id = provider_span.get("operation_id")
        if not isinstance(provider_id, str):
            fail(path, line, "provider_call is missing operation_id")
        operation_span = next(
            (span for span in spans if span.get("name") == "operation"), None
        )
        if operation_span is None:
            fail(
                path,
                line,
                "provider_call event does not include the enclosing operation in spans",
            )
        operation_id = operation_span.get("operation_id")
        if not isinstance(operation_id, str):
            fail(path, line, "enclosing operation is missing operation_id")
        if provider_id != operation_id:
            fail(
                path,
                line,
                "provider_call operation_id "
                f"{provider_id} does not match enclosing operation {operation_id}",
            )

    if fields.get("kind") == "audit":
        route = fields.get("route")
        if not isinstance(route, str):
            fail(path, line, "audit event is missing route")
        operation_id = fields.get("operation_id")
        if not isinstance(operation_id, str) or not operation_id.strip():
            fail(path, line, "audit event is missing a non-empty operation_id")
        facts.audit_events.append((route, operation_id))


def validate_file(path: Path, require_operation_id: bool) -> TraceFacts:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise InvalidTrace(f"{path}: cannot read: {error}") from error

    facts = TraceFacts()
    events = 0
    for offset, raw in enumerate(text.splitlines(), start=1):
        if not raw.strip():
            continue
        events += 1
        validate_line(path, offset, raw, facts, require_operation_id)
    if events == 0:
        fail(path, 1, "trace file has no events")
    return facts


def validate_attach_correlation(path: Path, facts: TraceFacts) -> None:
    create_ids = facts.operation_ids.get("create_volume", set())
    attach_ids = facts.operation_ids.get("attach_volume", set())
    if not create_ids:
        raise InvalidTrace(f"{path}: no create_volume operation span")
    if not attach_ids:
        raise InvalidTrace(f"{path}: no attach_volume operation span")

    has_create_audit = any(
        route == "/v2/volumes" and operation_id in create_ids
        for route, operation_id in facts.audit_events
    )
    if not has_create_audit:
        raise InvalidTrace(
            f"{path}: no create audit event on /v2/volumes carries a create_volume operation id"
        )
    has_attach_audit = any(
        route == "/v2/volumes/{volume_id}/attach" and operation_id in attach_ids
        for route, operation_id in facts.audit_events
    )
    if not has_attach_audit:
        raise InvalidTrace(
            f"{path}: no attach audit event on /v2/volumes/{{volume_id}}/attach "
            "carries an attach_volume operation id"
        )


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Validate committed JSON trace evidence for structure, span "
            "correlation and leaked secrets."
        )
    )
    parser.add_argument(
        "--require-operation-id",
        action="store_true",
        help="require a non-empty operation_id on every span (default: only "
        "operation and provider_call spans)",
    )
    parser.add_argument(
        "files",
        metavar="FILE",
        nargs="+",
        type=Path,
        help="one or more .jsonl trace files",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    failed = False
    for path in args.files:
        try:
            facts = validate_file(path, args.require_operation_id)
            if path.name == "attach.jsonl":
                validate_attach_correlation(path, facts)
        except InvalidTrace as error:
            print(error, file=sys.stderr)
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
