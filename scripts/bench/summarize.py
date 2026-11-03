#!/usr/bin/env python3
"""Render benchmark JSON records into CSV tables and a Markdown digest.

The report author copies the tables from TABLES.md and leaves the raw JSON
and CSV untouched. This tool never invents a value: a missing metric is
rendered as an empty cell or ``n/a``.
"""

from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path


def load(run_dir: Path) -> dict[str, dict]:
    records: dict[str, dict] = {}
    for path in sorted((run_dir / "json").glob("*.json")):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        records[record.get("scenario", path.stem)] = record
    return records


def num(value, digits: int = 3):
    if value is None:
        return ""
    if isinstance(value, float):
        return round(value, digits)
    return value


def res_stats(record: dict, key: str) -> dict:
    return ((record.get("resources") or {}).get(key) or {})


def write_csv(path: Path, header: list[str], rows: list[list]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle)
        writer.writerow(header)
        writer.writerows(rows)


def markdown_table(header: list[str], rows: list[list]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]
    for row in rows:
        lines.append("| " + " | ".join("" if cell is None else str(cell) for cell in row) + " |")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--run-dir", required=True)
    args = parser.parse_args()
    run_dir = Path(args.run_dir)
    summary_dir = run_dir / "summary"
    summary_dir.mkdir(parents=True, exist_ok=True)
    records = load(run_dir)

    micro_rows: list[list] = []
    micro_header = [
        "operation",
        "count",
        "errors",
        "min_ms",
        "mean_ms",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "max_ms",
    ]
    micro = records.get("micro")
    if micro:
        for op, stats in (micro.get("per_op") or {}).items():
            micro_rows.append(
                [
                    op,
                    stats.get("count", ""),
                    stats.get("errors", ""),
                    num(stats.get("min_ms")),
                    num(stats.get("mean_ms")),
                    num(stats.get("p50_ms")),
                    num(stats.get("p95_ms")),
                    num(stats.get("p99_ms")),
                    num(stats.get("max_ms")),
                ]
            )
    write_csv(summary_dir / "micro.csv", micro_header, micro_rows)

    storage_header = [
        "metric",
        "start_bytes",
        "end_bytes",
        "delta_bytes",
        "mutations",
        "bytes_per_mutation",
        "peak_bytes",
    ]
    storage_rows: list[list] = []
    state_scale_rows: list[list] = []
    if micro:
        setup = micro.get("setup") or {}
        for name in ("journal", "state"):
            block = setup.get(name) or {}
            storage_rows.append(
                [
                    name,
                    num(block.get("start_bytes")),
                    num(block.get("end_bytes")),
                    num(block.get("delta_bytes")),
                    num(block.get("mutations")),
                    num(block.get("bytes_per_mutation")),
                    num(block.get("peak_bytes")),
                ]
            )
        for sample in setup.get("state_scale") or []:
            state_scale_rows.append(
                [
                    sample.get("volumes", ""),
                    sample.get("state_bytes", ""),
                    sample.get("journal_bytes", ""),
                    num(sample.get("last_create_ms")),
                ]
            )
    write_csv(summary_dir / "journal-state.csv", storage_header, storage_rows)
    write_csv(
        summary_dir / "state-scale.csv",
        ["live_volumes", "state_bytes", "journal_bytes", "last_create_ms"],
        state_scale_rows,
    )

    concurrency_header = [
        "label",
        "concurrency",
        "admission_cap",
        "queue_timeout_s",
        "rate_limit_per_s",
        "duration_s",
        "ops",
        "ops_per_sec",
        "success_ops_per_sec",
        "error_rate",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "accepted_p95_ms",
        "read_p95_ms",
        "write_p95_ms",
        "http_503",
        "http_429",
        "network_errors",
        "rss_first_kib",
        "rss_last_kib",
        "rss_max_kib",
        "fds_first",
        "fds_last",
        "fds_max",
        "journal_delta_bytes",
        "state_delta_bytes",
        "state_max_bytes",
    ]
    concurrency_rows: list[list] = []
    for scenario in sorted(k for k in records if k.startswith("mixed-")):
        record = records[scenario]
        overall = record.get("overall") or {}
        read = record.get("read") or {}
        write = record.get("write") or {}
        status = record.get("status_counts") or {}
        rss = res_stats(record, "rss_kib")
        fds = res_stats(record, "fds")
        journal = res_stats(record, "journal_bytes")
        state = res_stats(record, "state_bytes")
        concurrency_rows.append(
            [
                scenario.removeprefix("mixed-"),
                record.get("concurrency", ""),
                record.get("admission_cap", ""),
                num(record.get("queue_timeout_seconds")),
                record.get("rate_limit_per_second", ""),
                num(record.get("duration_seconds")),
                record.get("ops", ""),
                num(record.get("throughput_ops_per_second")),
                num(record.get("successful_ops_per_second")),
                num(record.get("error_rate"), 6),
                num(overall.get("p50_ms")),
                num(overall.get("p95_ms")),
                num(overall.get("p99_ms")),
                num((record.get("accepted") or {}).get("p95_ms")),
                num(read.get("p95_ms")),
                num(write.get("p95_ms")),
                status.get("503", 0),
                status.get("429", 0),
                record.get("network_errors", 0),
                num(rss.get("first")),
                num(rss.get("last")),
                num(rss.get("max")),
                num(fds.get("first")),
                num(fds.get("last")),
                num(fds.get("max")),
                num(journal.get("delta")),
                num(state.get("delta")),
                num(state.get("max")),
            ]
        )
    write_csv(summary_dir / "concurrency.csv", concurrency_header, concurrency_rows)

    soak_header = [
        "duration_s",
        "workers",
        "ops",
        "ops_per_sec",
        "error_rate",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "rss_first_kib",
        "rss_last_kib",
        "rss_max_kib",
        "rss_kib_per_second",
        "fds_first",
        "fds_last",
        "fds_max",
        "fds_per_second",
        "journal_first_bytes",
        "journal_last_bytes",
        "journal_delta_bytes",
        "state_first_bytes",
        "state_last_bytes",
        "state_delta_bytes",
        "restarts",
        "restart_healthy_seconds",
    ]
    soak_rows: list[list] = []
    soak = records.get("soak")
    if soak:
        overall = soak.get("overall") or {}
        rss = res_stats(soak, "rss_kib")
        fds = res_stats(soak, "fds")
        journal = res_stats(soak, "journal_bytes")
        state = res_stats(soak, "state_bytes")
        restarts = soak.get("restarts") or []
        soak_rows.append(
            [
                num(soak.get("duration_seconds")),
                soak.get("workers", ""),
                soak.get("ops", ""),
                num(soak.get("throughput_ops_per_second")),
                num(soak.get("error_rate"), 6),
                num(overall.get("p50_ms")),
                num(overall.get("p95_ms")),
                num(overall.get("p99_ms")),
                num(rss.get("first")),
                num(rss.get("last")),
                num(rss.get("max")),
                num((soak.get("resources") or {}).get("rss_kib_per_second"), 5),
                num(fds.get("first")),
                num(fds.get("last")),
                num(fds.get("max")),
                num((soak.get("resources") or {}).get("fds_per_second"), 7),
                num(journal.get("first")),
                num(journal.get("last")),
                num(journal.get("delta")),
                num(state.get("first")),
                num(state.get("last")),
                num(state.get("delta")),
                len(restarts),
                num(sum(r.get("healthy_seconds", 0) for r in restarts)),
            ]
        )
    write_csv(summary_dir / "soak.csv", soak_header, soak_rows)

    restart_header = [
        "scenario",
        "iteration",
        "journal_bytes_before",
        "state_bytes_before",
        "stop_seconds",
        "healthy_seconds",
        "state_bytes_after",
        "journal_bytes_after",
        "listed_volumes",
        "missing_volumes",
    ]
    restart_rows: list[list] = []
    for scenario in sorted(k for k in records if k.startswith("restart-")):
        record = records[scenario]
        for row in record.get("records") or []:
            before = row.get("before") or {}
            after = row.get("after") or {}
            restart_rows.append(
                [
                    scenario,
                    row.get("iteration", ""),
                    num(before.get("journal_bytes")),
                    num(before.get("state_bytes")),
                    num(row.get("stop_seconds")),
                    num(row.get("healthy_seconds")),
                    num(after.get("state_bytes")),
                    num(after.get("journal_bytes")),
                    num(after.get("listed_volumes")),
                    ",".join(after.get("missing_volumes") or []),
                ]
            )
    soak_record = records.get("soak")
    if soak_record:
        for index, row in enumerate(soak_record.get("restarts") or []):
            after = row.get("after") or {}
            restart_rows.append(
                [
                    "soak-restart",
                    index + 1,
                    num(row.get("journal_bytes")),
                    num(row.get("state_bytes")),
                    num(row.get("stop_seconds")),
                    num(row.get("healthy_seconds")),
                    num(after.get("state_bytes")),
                    num(after.get("journal_bytes")),
                    num(after.get("listed_volumes")),
                    ",".join(after.get("missing_volumes") or []),
                ]
            )
    write_csv(summary_dir / "restart.csv", restart_header, restart_rows)

    tables = [
        "# Benchmark tables",
        "",
        "Generated from the raw JSON records by `scripts/bench/summarize.py`.",
        "Percentiles are nearest-rank over every recorded request; the CSV",
        "`*-ops.csv` files are a deterministic latency sample (first 5,000 per",
        "operation).",
        "",
        "## Micro (concurrency 1)",
        "",
        markdown_table(micro_header, micro_rows),
        "",
        "## Journal and state-file measurements (derived; not separately timed)",
        "",
        markdown_table(storage_header, storage_rows),
        "",
        "### State-file size versus live volumes",
        "",
        markdown_table(
            ["live_volumes", "state_bytes", "journal_bytes", "last_create_ms"],
            state_scale_rows,
        ),
        "",
        "## Mixed concurrency sweep",
        "",
        markdown_table(concurrency_header, concurrency_rows),
        "",
        "## Shortened soak",
        "",
        markdown_table(soak_header, soak_rows),
        "",
        "## Restart replay",
        "",
        markdown_table(restart_header, restart_rows),
        "",
    ]
    (summary_dir / "TABLES.md").write_text("\n".join(tables), encoding="utf-8")
    print(f"summary written: {summary_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
