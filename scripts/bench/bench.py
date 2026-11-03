#!/usr/bin/env python3
"""Reproducible benchmark driver for Volvisor issue #43.

The driver speaks the real Volume API v2 over HTTP, keeps bearer tokens in
an owner-only file (never in argv or output), and records exact counts and
latencies. It does not mock the provider: the daemon runs the real LVM
provider over the lab's loop-backed VG with the production journal/state
fsync path.

Scenarios:
  micro      one-operation-at-a-time create/delete, attach/report/detach,
             grow, inspect, list and capabilities
  mixed      concurrency sweep (1/8/32/128) with mixed reads and writes
  soak       shortened steady-state run with attach/detach/grow and restart
  restart    measure journal/state replay to a healthy listener

Outputs under --out-dir:
  json/<scenario>.json     exact aggregates (including all-request percentiles)
  csv/<scenario>-ops.csv   per-operation latency sample (deterministic prefix)
  csv/<scenario>-timeline.csv  per-second counters/resources (soak/mixed)
"""

from __future__ import annotations

import argparse
import csv
import http.client
import json
import math
import os
import random
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

API_VERSION = "volvisor.volume.v2"
LATENCY_SAMPLE_LIMIT = 5000
DEFAULT_TIMEOUT = 180.0


# --------------------------------------------------------------------------
# Small utilities
# --------------------------------------------------------------------------


def now_utc() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def percentile(values: list[float], p: float) -> float | None:
    """Nearest-rank percentile (rank = ceil(p/100 * n))."""
    if not values:
        return None
    ordered = sorted(values)
    rank = max(0, min(len(ordered) - 1, math.ceil(p / 100.0 * len(ordered)) - 1))
    return ordered[rank]


def latency_summary(values: list[float]) -> dict:
    if not values:
        return {"count": 0}
    return {
        "count": len(values),
        "min_ms": round(min(values), 3),
        "mean_ms": round(sum(values) / len(values), 3),
        "p50_ms": round(percentile(values, 50), 3),
        "p95_ms": round(percentile(values, 95), 3),
        "p99_ms": round(percentile(values, 99), 3),
        "max_ms": round(max(values), 3),
    }


def file_size(path: str | None) -> int | None:
    if not path:
        return None
    try:
        return os.path.getsize(path)
    except OSError:
        return None


def proc_stats(pid: int | None) -> tuple[int | None, int | None]:
    """Return (VmRSS KiB, open fd count) for a live process."""
    if not pid:
        return None, None
    rss = None
    fds = None
    try:
        with open(f"/proc/{pid}/status", encoding="ascii") as handle:
            for line in handle:
                if line.startswith("VmRSS:"):
                    rss = int(line.split()[1])
                    break
    except OSError:
        return None, None
    try:
        fds = len(os.listdir(f"/proc/{pid}/fd"))
    except OSError:
        fds = None
    return rss, fds


def read_pid(path: str | None) -> int | None:
    if not path:
        return None
    try:
        return int(Path(path).read_text(encoding="ascii").strip())
    except (OSError, ValueError):
        return None


def git_identity(repo: str) -> tuple[str, bool]:
    sha = subprocess.run(
        ["git", "-C", repo, "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.strip() or "unknown"
    dirty = bool(
        subprocess.run(
            ["git", "-C", repo, "status", "--porcelain"],
            capture_output=True,
            text=True,
            check=False,
        ).stdout.strip()
    )
    return sha, dirty


def linear_slope(points: list[tuple[float, float]]) -> float | None:
    """Least-squares slope (y units per second) for sampled pairs."""
    if len(points) < 2:
        return None
    n = float(len(points))
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    denom = sum((x - mean_x) ** 2 for x, _ in points)
    if denom == 0:
        return None
    return sum((x - mean_x) * (y - mean_y) for x, y in points) / denom


# --------------------------------------------------------------------------
# HTTP client
# --------------------------------------------------------------------------


class Result:
    __slots__ = ("status", "body", "error_code", "latency_ms", "error")

    def __init__(
        self,
        status: int,
        body: dict | None,
        error_code: str | None,
        latency_ms: float,
        error: str | None = None,
    ) -> None:
        self.status = status
        self.body = body
        self.error_code = error_code
        self.latency_ms = latency_ms
        self.error = error

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300


class HttpClient:
    """One keep-alive connection per thread; a broken connection is recorded
    as an error and reopened for the next request (never silently retried)."""

    def __init__(self, host: str, port: int, token: str, timeout: float = DEFAULT_TIMEOUT):
        self.host = host
        self.port = port
        self.token = token
        self.timeout = timeout
        self._local = threading.local()

    def _connection(self) -> http.client.HTTPConnection:
        conn = getattr(self._local, "conn", None)
        if conn is None:
            conn = http.client.HTTPConnection(self.host, self.port, timeout=self.timeout)
            self._local.conn = conn
        return conn

    def _drop(self) -> None:
        conn = getattr(self._local, "conn", None)
        if conn is not None:
            try:
                conn.close()
            except OSError:
                pass
        self._local.conn = None

    def request(self, method: str, path: str, body: dict | None = None) -> Result:
        data = None
        headers = {"authorization": f"Bearer {self.token}"}
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode()
            headers["content-type"] = "application/json"
        start = time.perf_counter_ns()
        try:
            conn = self._connection()
            conn.request(method, path, body=data, headers=headers)
            response = conn.getresponse()
            raw = response.read()
            latency_ms = (time.perf_counter_ns() - start) / 1_000_000.0
            if response.will_close:
                self._drop()
            parsed = None
            error_code = None
            if raw:
                try:
                    parsed = json.loads(raw)
                except json.JSONDecodeError:
                    parsed = {"raw": raw.decode("utf-8", "replace")}
            if not 200 <= response.status < 300:
                if isinstance(parsed, dict):
                    error_code = parsed.get("code")
            return Result(response.status, parsed, error_code, latency_ms)
        except (OSError, http.client.HTTPException) as exc:
            latency_ms = (time.perf_counter_ns() - start) / 1_000_000.0
            self._drop()
            return Result(0, None, "NETWORK_ERROR", latency_ms, f"{type(exc).__name__}: {exc}")


# --------------------------------------------------------------------------
# Recorder
# --------------------------------------------------------------------------


class Recorder:
    def __init__(self, out_dir: Path, scenario: str) -> None:
        self.out_dir = out_dir
        self.scenario = scenario
        self.lock = threading.Lock()
        self.latencies: dict[str, list[float]] = {}
        self.ok_latencies: list[float] = []
        self.op_errors: dict[str, int] = {}
        self.samples: dict[str, list[tuple[float, int, str | None]]] = {}
        self.status_counts: dict[str, int] = {}
        self.error_codes: dict[str, int] = {}
        self.ops = 0
        self.errors = 0
        self.reads = 0
        self.writes = 0
        self.network_errors = 0
        self.network_error_kinds: dict[str, int] = {}

    def record(self, op: str, kind: str, result: Result) -> None:
        with self.lock:
            self.ops += 1
            if kind == "read":
                self.reads += 1
            else:
                self.writes += 1
            self.latencies.setdefault(op, []).append(result.latency_ms)
            if result.ok:
                self.ok_latencies.append(result.latency_ms)
            status_key = str(result.status)
            self.status_counts[status_key] = self.status_counts.get(status_key, 0) + 1
            if not result.ok:
                self.errors += 1
                self.op_errors[op] = self.op_errors.get(op, 0) + 1
                if result.status == 0:
                    self.network_errors += 1
                    kind = (result.error or "unknown").split(":", 1)[0]
                    self.network_error_kinds[kind] = self.network_error_kinds.get(kind, 0) + 1
                code = result.error_code or "HTTP_ERROR"
                self.error_codes[code] = self.error_codes.get(code, 0) + 1
            samples = self.samples.setdefault(op, [])
            if len(samples) < LATENCY_SAMPLE_LIMIT:
                samples.append((time.time(), round(result.latency_ms, 3), result.status))

    def snapshot(self) -> tuple[int, int, int, int, dict[str, int], dict[str, int]]:
        with self.lock:
            return (
                self.ops,
                self.errors,
                self.reads,
                self.writes,
                dict(self.status_counts),
                dict(self.error_codes),
            )

    def summary(self) -> dict:
        with self.lock:
            overall = []
            per_op = {}
            for op, values in self.latencies.items():
                overall.extend(values)
                per_op[op] = latency_summary(values)
                per_op[op]["errors"] = self.op_errors.get(op, 0)
            reads = [v for op, vals in self.latencies.items() if op in READ_OPS for v in vals]
            writes = [v for op, vals in self.latencies.items() if op not in READ_OPS for v in vals]
            return {
                "ops": self.ops,
                "errors": self.errors,
                "error_rate": round(self.errors / self.ops, 6) if self.ops else None,
                "network_errors": self.network_errors,
                "network_error_kinds": dict(sorted(self.network_error_kinds.items())),
                "reads": self.reads,
                "writes": self.writes,
                "status_counts": dict(sorted(self.status_counts.items())),
                "error_codes": dict(sorted(self.error_codes.items())),
                "overall": latency_summary(overall),
                "accepted": latency_summary(self.ok_latencies),
                "read": latency_summary(reads),
                "write": latency_summary(writes),
                "per_op": dict(sorted(per_op.items())),
            }

    def write_csv(self) -> None:
        path = self.out_dir / "csv" / f"{self.scenario}-ops.csv"
        path.parent.mkdir(parents=True, exist_ok=True)
        with self.lock:
            samples = {op: list(rows) for op, rows in self.samples.items()}
        with path.open("w", newline="", encoding="utf-8") as handle:
            writer = csv.writer(handle)
            writer.writerow(["epoch_s", "op", "latency_ms", "status"])
            for op in sorted(samples):
                for epoch, latency, status in samples[op]:
                    writer.writerow([f"{epoch:.3f}", op, f"{latency:.3f}", status])


READ_OPS = {
    "inspect",
    "list",
    "capabilities",
    "healthz",
    "metrics",
}


# --------------------------------------------------------------------------
# Resource sampler
# --------------------------------------------------------------------------


class Sampler(threading.Thread):
    def __init__(
        self,
        out_csv: Path,
        journal: str | None,
        state: str | None,
        recorder: Recorder,
        stop: threading.Event,
        pid: int | None = None,
        pidfile: str | None = None,
        interval: float = 1.0,
    ) -> None:
        super().__init__(daemon=True)
        self.out_csv = out_csv
        self.journal = journal
        self.state = state
        self.recorder = recorder
        self.stop = stop
        self.pid = pid
        self.pidfile = pidfile
        self.interval = interval
        self.rows: list[dict] = []

    def run(self) -> None:
        self.out_csv.parent.mkdir(parents=True, exist_ok=True)
        start = time.time()
        with self.out_csv.open("w", newline="", encoding="utf-8") as handle:
            writer = csv.writer(handle)
            writer.writerow(
                [
                    "epoch_s",
                    "elapsed_s",
                    "ops",
                    "errors",
                    "reads",
                    "writes",
                    "rss_kib",
                    "fds",
                    "journal_bytes",
                    "state_bytes",
                ]
            )
            while not self.stop.wait(self.interval):
                pid = read_pid(self.pidfile) if self.pidfile else self.pid
                rss, fds = proc_stats(pid)
                ops, errors, reads, writes, _, _ = self.recorder.snapshot()
                row = {
                    "epoch_s": time.time(),
                    "elapsed_s": time.time() - start,
                    "ops": ops,
                    "errors": errors,
                    "reads": reads,
                    "writes": writes,
                    "rss_kib": rss,
                    "fds": fds,
                    "journal_bytes": file_size(self.journal),
                    "state_bytes": file_size(self.state),
                }
                self.rows.append(row)
                writer.writerow(
                    [
                        f"{row['epoch_s']:.3f}",
                        f"{row['elapsed_s']:.3f}",
                        row["ops"],
                        row["errors"],
                        row["reads"],
                        row["writes"],
                        "" if rss is None else rss,
                        "" if fds is None else fds,
                        "" if row["journal_bytes"] is None else row["journal_bytes"],
                        "" if row["state_bytes"] is None else row["state_bytes"],
                    ]
                )
                handle.flush()


def resource_summary(sampler: Sampler) -> dict:
    rows = sampler.rows
    if not rows:
        return {"samples": 0}

    def series(key: str) -> list[float]:
        return [float(row[key]) for row in rows if row[key] is not None]

    rss = series("rss_kib")
    fds = series("fds")
    journal = series("journal_bytes")
    state = series("state_bytes")
    def stats(values: list[float]) -> dict | None:
        if not values:
            return None
        return {
            "first": values[0],
            "last": values[-1],
            "min": min(values),
            "max": max(values),
            "delta": values[-1] - values[0],
        }

    return {
        "samples": len(rows),
        "rss_kib": stats(rss),
        "fds": stats(fds),
        "journal_bytes": stats(journal),
        "state_bytes": stats(state),
        "rss_kib_per_second": (
            round(
                linear_slope(
                    [
                        (float(row["elapsed_s"]), float(row["rss_kib"]))
                        for row in rows
                        if row["rss_kib"] is not None
                    ]
                ),
                4,
            )
            if len(rss) > 1
            else None
        ),
        "fds_per_second": (
            round(
                linear_slope(
                    [
                        (float(row["elapsed_s"]), float(row["fds"]))
                        for row in rows
                        if row["fds"] is not None
                    ]
                ),
                6,
            )
            if len(fds) > 1
            else None
        ),
    }


# --------------------------------------------------------------------------
# API helpers
# --------------------------------------------------------------------------

# A shed response means the client should slow down instead of spinning on
# the rejected request. The delay is deliberately coarse: the probe measures
# shedding visibility, not a retry policy.
SHED_BACKOFF_SECONDS = 1.0
NETWORK_BACKOFF_SECONDS = 0.25


def record_result(
    recorder: Recorder, client: HttpClient, op: str, kind: str, result: Result
) -> None:
    recorder.record(op, kind, result)
    if result.status in (429, 503):
        # An admission rejection can arrive before the request body is
        # drained; the server may then close the keep-alive connection.
        # Reconnect next time instead of writing a request into a half-closed
        # socket.
        client._drop()
        time.sleep(SHED_BACKOFF_SECONDS)
    elif result.status == 0:
        time.sleep(NETWORK_BACKOFF_SECONDS)


def create_volume(
    client: HttpClient, volume_id: str, size_bytes: int, operation_id: str
) -> Result:
    return client.request(
        "POST",
        "/v2/volumes",
        {
            "api_version": API_VERSION,
            "operation_id": operation_id,
            "project_id": "bench",
            "volume_id": volume_id,
            "class": "native-local",
            "size_bytes": size_bytes,
        },
    )


def delete_volume(client: HttpClient, volume_id: str, generation: int, operation_id: str) -> Result:
    return client.request(
        "DELETE",
        f"/v2/volumes/{volume_id}",
        {
            "api_version": API_VERSION,
            "operation_id": operation_id,
            "expected_generation": generation,
            "data_erasure_policy": "retain",
        },
    )


def inspect_volume(client: HttpClient, volume_id: str) -> Result:
    return client.request("GET", f"/v2/volumes/{volume_id}")


def list_volumes(client: HttpClient, limit: int = 100) -> Result:
    return client.request("GET", f"/v2/volumes?limit={limit}")


def capabilities(client: HttpClient) -> Result:
    return client.request("GET", "/v2/capabilities")


def attach_volume(
    client: HttpClient,
    volume_id: str,
    vm_id: str,
    attachment_id: str,
    expected_generation: int,
    operation_id: str,
) -> Result:
    return client.request(
        "POST",
        f"/v2/volumes/{volume_id}/attach",
        {
            "api_version": API_VERSION,
            "operation_id": operation_id,
            "vm_id": vm_id,
            "host_id": "bench-host",
            "attachment_id": attachment_id,
            "expected_volume_generation": expected_generation,
            "access_mode": "single_writer",
            "requested_frontend": "virtio-blk",
        },
    )


def report_active(
    client: HttpClient,
    volume_id: str,
    attachment_id: str,
    attachment_generation: int,
    operation_id: str,
) -> Result:
    return client.request(
        "POST",
        f"/v2/volumes/{volume_id}/attachment-state",
        {
            "api_version": API_VERSION,
            "operation_id": operation_id,
            "attachment_id": attachment_id,
            "expected_attachment_generation": attachment_generation,
            "state": "active",
            "evidence": "guest_confirmed",
            "reporter": "bench-driver",
        },
    )


def detach_volume(
    client: HttpClient,
    volume_id: str,
    attachment_id: str,
    attachment_generation: int,
    operation_id: str,
) -> Result:
    return client.request(
        "POST",
        f"/v2/volumes/{volume_id}/detach",
        {
            "api_version": API_VERSION,
            "operation_id": operation_id,
            "attachment_id": attachment_id,
            "expected_attachment_generation": attachment_generation,
            "vm_stopped_or_io_drained_proof": "io_drained",
        },
    )


def grow_volume(
    client: HttpClient, volume_id: str, new_size: int, generation: int, operation_id: str
) -> Result:
    return client.request(
        "POST",
        f"/v2/volumes/{volume_id}/grow",
        {
            "api_version": API_VERSION,
            "operation_id": operation_id,
            "new_size_bytes": new_size,
            "expected_generation": generation,
        },
    )


class Ids:
    def __init__(self, prefix: str) -> None:
        self.prefix = prefix
        self.counter = 0
        self.lock = threading.Lock()

    def next(self, label: str) -> str:
        with self.lock:
            self.counter += 1
            return f"{self.prefix}-{label}-{self.counter}"


# --------------------------------------------------------------------------
# Scenario: micro
# --------------------------------------------------------------------------


def run_micro(client: HttpClient, recorder: Recorder, ids: Ids, args: argparse.Namespace) -> dict:
    setup: dict = {}
    size = args.volume_size
    journal_start = file_size(args.journal)
    state_start = file_size(args.state)
    state_peak = state_start or 0
    state_scale: list[dict] = []

    # create/delete, one pair at a time.
    for index in range(args.create_delete_iterations):
        volume_id = f"micro-cd-{index}"
        created = create_volume(client, volume_id, size, ids.next("create"))
        recorder.record("create", "write", created)
        if created.ok:
            generation = created.body["generation"]
            deleted = delete_volume(client, volume_id, generation, ids.next("delete"))
            recorder.record("delete", "write", deleted)
        else:
            # Cleanup a possibly-created volume on an ambiguous failure.
            inspected = inspect_volume(client, volume_id)
            if inspected.ok:
                delete_volume(
                    client, volume_id, inspected.body["generation"], ids.next("delete-recover")
                )

    # attach -> report -> detach cycles on one volume.
    cycle_id = "micro-cycle"
    created = create_volume(client, cycle_id, 64 * 1024 * 1024, ids.next("create-cycle"))
    setup["cycle_create_status"] = created.status
    if not created.ok:
        raise RuntimeError(f"cycle volume create failed: {created.status} {created.body}")
    generation = created.body["generation"]
    for index in range(args.cycle_iterations):
        attachment_id = f"micro-att-{index}"
        attached = attach_volume(
            client,
            cycle_id,
            "bench-vm",
            attachment_id,
            generation,
            ids.next("attach"),
        )
        recorder.record("attach", "write", attached)
        if not attached.ok:
            inspected = inspect_volume(client, cycle_id)
            if inspected.ok:
                generation = inspected.body["generation"]
                attachment = (inspected.body.get("attachments") or [None])[0]
                if attachment is not None:
                    detach_volume(
                        client,
                        cycle_id,
                        attachment["attachment_id"],
                        attachment["attachment_generation"],
                        ids.next("detach-recover"),
                    )
                    inspected = inspect_volume(client, cycle_id)
                    if inspected.ok:
                        generation = inspected.body["generation"]
            continue
        generation = attached.body["volume_generation"]
        attachment_generation = attached.body["attachment_generation"]
        reported = report_active(
            client, cycle_id, attachment_id, attachment_generation, ids.next("report")
        )
        recorder.record("report", "write", reported)
        detached = detach_volume(
            client, cycle_id, attachment_id, attachment_generation, ids.next("detach")
        )
        recorder.record("detach", "write", detached)
        if detached.ok:
            generation = detached.body["generation"]
        else:
            inspected = inspect_volume(client, cycle_id)
            if inspected.ok:
                generation = inspected.body["generation"]

    # grow, one increment at a time (inspect supplies the refreshed generation).
    grow_id = "micro-grow"
    grow_size = 64 * 1024 * 1024
    created = create_volume(client, grow_id, grow_size, ids.next("create-grow"))
    setup["grow_create_status"] = created.status
    if not created.ok:
        raise RuntimeError(f"grow volume create failed: {created.status} {created.body}")
    generation = created.body["generation"]
    current = grow_size
    for _ in range(args.grow_iterations):
        current += args.grow_increment
        grown = grow_volume(client, grow_id, current, generation, ids.next("grow"))
        recorder.record("grow", "write", grown)
        inspected = inspect_volume(client, grow_id)
        if inspected.ok:
            generation = inspected.body["generation"]
        else:
            break

    # State-file scaling: every LVM mutation rewrites and fsyncs the whole
    # JSON state file. Measure create latency and the file size as the
    # number of live volumes grows; this is end-to-end, not an isolated
    # state-save timing.
    scale_ids: list[str] = []
    for index in range(args.scale_volumes):
        volume_id = f"micro-scale-{index}"
        created = create_volume(client, volume_id, size, ids.next("scale-create"))
        recorder.record("create", "write", created)
        if not created.ok:
            break
        scale_ids.append(volume_id)
        state_bytes = file_size(args.state) or 0
        state_peak = max(state_peak, state_bytes)
        if index + 1 in (1, 8, 32):
            state_scale.append(
                {
                    "volumes": index + 1,
                    "state_bytes": state_bytes,
                    "journal_bytes": file_size(args.journal),
                    "last_create_ms": round(created.latency_ms, 3),
                }
            )
    for volume_id in scale_ids:
        inspected = inspect_volume(client, volume_id)
        if not inspected.ok:
            continue
        deleted = delete_volume(
            client, volume_id, inspected.body["generation"], ids.next("scale-delete")
        )
        recorder.record("delete", "write", deleted)

    # read-only operations.
    for _ in range(args.inspect_iterations):
        recorder.record("inspect", "read", inspect_volume(client, cycle_id))
    for _ in range(args.list_iterations):
        recorder.record("list", "read", list_volumes(client))
    for _ in range(args.capabilities_iterations):
        recorder.record("capabilities", "read", capabilities(client))

    # Cleanup (not part of the recorded timings).
    for volume_id in (cycle_id, grow_id):
        inspected = inspect_volume(client, volume_id)
        if not inspected.ok:
            continue
        body = inspected.body
        for attachment in body.get("attachments") or []:
            detach_volume(
                client,
                volume_id,
                attachment["attachment_id"],
                attachment["attachment_generation"],
                ids.next("cleanup-detach"),
            )
            inspected = inspect_volume(client, volume_id)
            body = inspected.body if inspected.ok else body
        delete_volume(client, volume_id, body["generation"], ids.next("cleanup-delete"))

    setup["state_scale"] = state_scale
    setup["state_peak_bytes"] = state_peak
    setup["journal_start_bytes"] = journal_start
    setup["state_start_bytes"] = state_start
    return setup


# --------------------------------------------------------------------------
# Scenario: mixed concurrency sweep
# --------------------------------------------------------------------------


class WorkerState:
    def __init__(self, volume_id: str, size: int, grows: int) -> None:
        self.volume_id = volume_id
        self.size = size
        self.generation = 1
        self.attachment_id: str | None = None
        self.attachment_generation = 0
        self.stage = 0  # 0 detached, 1 attached-prepared, 2 attached-active
        self.grows_left = grows
        self.attach_seq = 0


def reconcile(client: HttpClient, volume_id: str, state: WorkerState, recorder: Recorder) -> bool:
    inspected = inspect_volume(client, volume_id)
    record_result(recorder, client, "inspect", "read", inspected)
    if not inspected.ok:
        return False
    body = inspected.body
    state.generation = body["generation"]
    state.size = body["provisioned_bytes"]
    attachments = body.get("attachments") or []
    if not attachments:
        state.stage = 0
        state.attachment_id = None
        state.attachment_generation = 0
    else:
        attachment = attachments[0]
        state.attachment_id = attachment["attachment_id"]
        state.attachment_generation = attachment["attachment_generation"]
        state.stage = 2 if attachment["state"] == "active" else 1
    return True


def mixed_worker(
    client: HttpClient,
    recorder: Recorder,
    ids: Ids,
    state: WorkerState,
    stop_ts: float,
    stop_event: threading.Event,
    read_ratio: float,
    grow_increment: int,
    seed: int,
) -> None:
    rng = random.Random(seed)
    vm_id = f"bench-vm-{state.volume_id}"
    while time.monotonic() < stop_ts and not stop_event.is_set():
        if rng.random() < read_ratio:
            choice = rng.random()
            if choice < 0.75:
                record_result(recorder, client, "inspect", "read", inspect_volume(client, state.volume_id))
            elif choice < 0.90:
                record_result(recorder, client, "list", "read", list_volumes(client))
            else:
                record_result(recorder, client, "capabilities", "read", capabilities(client))
            continue

        if state.stage == 0:
            state.attach_seq += 1
            attachment_id = f"mix-{state.volume_id}-{state.attach_seq}"
            attached = attach_volume(
                client,
                state.volume_id,
                vm_id,
                attachment_id,
                state.generation,
                ids.next("attach"),
            )
            record_result(recorder, client, "attach", "write", attached)
            if attached.ok:
                state.generation = attached.body["volume_generation"]
                state.attachment_id = attachment_id
                state.attachment_generation = attached.body["attachment_generation"]
                state.stage = 1
            else:
                reconcile(client, state.volume_id, state, recorder)
        elif state.stage == 1:
            reported = report_active(
                client,
                state.volume_id,
                state.attachment_id,
                state.attachment_generation,
                ids.next("report"),
            )
            record_result(recorder, client, "report", "write", reported)
            if reported.ok:
                state.stage = 2
            else:
                reconcile(client, state.volume_id, state, recorder)
        elif state.grows_left > 0 and rng.random() < 0.2:
            new_size = state.size + grow_increment
            grown = grow_volume(
                client, state.volume_id, new_size, state.generation, ids.next("grow")
            )
            record_result(recorder, client, "grow", "write", grown)
            state.grows_left -= 1
            reconcile(client, state.volume_id, state, recorder)
        else:
            detached = detach_volume(
                client,
                state.volume_id,
                state.attachment_id,
                state.attachment_generation,
                ids.next("detach"),
            )
            record_result(recorder, client, "detach", "write", detached)
            if detached.ok:
                state.generation = detached.body["generation"]
                state.stage = 0
            else:
                reconcile(client, state.volume_id, state, recorder)


def cleanup_volume(client: HttpClient, volume_id: str, ids: Ids) -> list[str]:
    failures: list[str] = []
    inspected = inspect_volume(client, volume_id)
    if not inspected.ok:
        return [f"{volume_id}: inspect {inspected.status}"]
    body = inspected.body
    for attachment in body.get("attachments") or []:
        detached = detach_volume(
            client,
            volume_id,
            attachment["attachment_id"],
            attachment["attachment_generation"],
            ids.next("cleanup-detach"),
        )
        if not detached.ok:
            failures.append(f"{volume_id}: detach {detached.status} {detached.error_code}")
            return failures
        inspected = inspect_volume(client, volume_id)
        if not inspected.ok:
            failures.append(f"{volume_id}: re-inspect {inspected.status}")
            return failures
        body = inspected.body
    deleted = delete_volume(client, volume_id, body["generation"], ids.next("cleanup-delete"))
    if not deleted.ok:
        failures.append(f"{volume_id}: delete {deleted.status} {deleted.error_code}")
    return failures


def run_mixed(client: HttpClient, args: argparse.Namespace, out_dir: Path) -> dict:
    label = args.label or f"c{args.concurrency}"
    scenario = f"mixed-{label}"
    recorder = Recorder(out_dir, scenario)
    ids = Ids(f"mix-{label}")
    volume_size = args.volume_size
    states = [
        WorkerState(f"mix-{label}-v{worker}", volume_size, grows=4)
        for worker in range(args.concurrency)
    ]

    setup_failures: list[str] = []
    for state in states:
        created = create_volume(client, state.volume_id, volume_size, ids.next("precreate"))
        if not created.ok:
            setup_failures.append(f"{state.volume_id}: {created.status} {created.error_code}")
        else:
            state.generation = created.body["generation"]
    if setup_failures:
        summary = {
            "scenario": scenario,
            "status": "SETUP_FAILED",
            "setup_failures": setup_failures[:20],
            "admission_cap": args.admission_cap,
            "queue_timeout_seconds": args.queue_timeout,
            "rate_limit_per_second": args.rate_limit,
        }
        return summary

    stop_event = threading.Event()
    sampler = Sampler(
        out_dir / "csv" / f"{scenario}-timeline.csv",
        args.journal,
        args.state,
        recorder,
        stop_event,
        pid=args.daemon_pid,
        interval=args.sample_seconds,
    )
    start_mono = time.monotonic()
    start_wall = time.time()
    stop_ts = start_mono + args.duration
    sampler.start()
    threads = []
    for index, state in enumerate(states):
        thread = threading.Thread(
            target=mixed_worker,
            name=f"mixed-{index}",
            args=(
                client,
                recorder,
                ids,
                state,
                stop_ts,
                stop_event,
                args.read_ratio,
                args.grow_increment,
                args.seed + index,
            ),
            daemon=True,
        )
        thread.start()
        threads.append(thread)
    for thread in threads:
        thread.join(timeout=args.duration + DEFAULT_TIMEOUT)
    elapsed = time.monotonic() - start_mono
    stop_event.set()
    sampler.join(timeout=5)

    cleanup_failures: list[str] = []
    for state in states:
        cleanup_failures.extend(cleanup_volume(client, state.volume_id, ids))

    summary = recorder.summary()
    summary.update(
        {
            "scenario": scenario,
            "status": "OK",
            "concurrency": args.concurrency,
            "duration_seconds": round(elapsed, 3),
            "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(start_wall)),
            "throughput_ops_per_second": round(recorder.ops / elapsed, 3) if elapsed else None,
            "successful_ops_per_second": (
                round((recorder.ops - recorder.errors) / elapsed, 3) if elapsed else None
            ),
            "read_ratio_target": args.read_ratio,
            "read_ratio_actual": round(recorder.reads / recorder.ops, 4) if recorder.ops else None,
            "admission_cap": args.admission_cap,
            "queue_timeout_seconds": args.queue_timeout,
            "rate_limit_per_second": args.rate_limit,
            "resources": resource_summary(sampler),
            "cleanup_failures": cleanup_failures[:20],
        }
    )
    return summary


# --------------------------------------------------------------------------
# Scenario: soak
# --------------------------------------------------------------------------


def health_ok(host: str, port: int, timeout: float = 1.0) -> bool:
    try:
        conn = http.client.HTTPConnection(host, port, timeout=timeout)
        conn.request("GET", "/healthz")
        response = conn.getresponse()
        response.read()
        conn.close()
        return response.status == 200
    except (OSError, http.client.HTTPException):
        return False


def replay_snapshot(client: HttpClient, args: argparse.Namespace, expected: list[str] | None = None) -> dict:
    """Observe the state a restarted daemon exposes after journal replay."""
    snapshot: dict = {
        "state_bytes": file_size(args.state),
        "journal_bytes": file_size(args.journal),
    }
    listed = list_volumes(client, limit=1000)
    # A keep-alive connection opened before the restart can fail once while
    # the listener is new; retry only that transport-level failure.
    for _ in range(2):
        if listed.status != 0:
            break
        time.sleep(0.05)
        listed = list_volumes(client, limit=1000)
    snapshot["list_status"] = listed.status
    if listed.ok and isinstance(listed.body, dict):
        visible = [item.get("volume_id") for item in listed.body.get("volumes") or []]
        snapshot["listed_volumes"] = len(visible)
        if expected is not None:
            snapshot["expected_volumes"] = len(expected)
            snapshot["missing_volumes"] = sorted(set(expected) - set(visible))
    return snapshot


def wait_healthy(host: str, port: int, timeout: float) -> float:
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        if health_ok(host, port):
            return time.monotonic() - start
        time.sleep(0.02)
    raise RuntimeError(f"daemon did not become healthy within {timeout}s")


# Daemons spawned by this process are kept so their exit is reaped (a
# zombie would otherwise make the liveness check in stop_daemon hang).
CHILDREN: dict[int, subprocess.Popen] = {}


def stop_daemon(pid: int, timeout: float = 30.0) -> float:
    start = time.monotonic()
    child = CHILDREN.pop(pid, None)
    if child is not None:
        child.terminate()
        try:
            child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=5)
        return time.monotonic() - start
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return 0.0
    while time.monotonic() - start < timeout:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return time.monotonic() - start
        time.sleep(0.02)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    return time.monotonic() - start


def start_daemon(binary: str, config: str, log_path: str) -> tuple[int, float]:
    handle = open(log_path, "a", encoding="utf-8")
    process = subprocess.Popen(
        [binary, "--config", config],
        stdout=handle,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    CHILDREN[process.pid] = process
    return process.pid, time.monotonic()


def soak_worker(
    client: HttpClient,
    recorder: Recorder,
    ids: Ids,
    volume_id: str,
    stop_ts: float,
    stop_event: threading.Event,
    read_ratio: float,
    grow_increment: int,
    max_extra_grows: int,
    seed: int,
) -> None:
    rng = random.Random(seed)
    grows_left = max_extra_grows
    vm_id = f"bench-soak-vm-{volume_id}"
    attach_seq = 0
    while time.monotonic() < stop_ts and not stop_event.is_set():
        # The soak prioritizes honest state reconciliation over raw rate:
        # every write is preceded by an inspect, so a restart mid-cycle never
        # turns into a stream of fabricated failures.
        inspected = inspect_volume(client, volume_id)
        recorder.record("inspect", "read", inspected)
        if not inspected.ok:
            time.sleep(0.05)
            continue
        body = inspected.body
        generation = body["generation"]
        size = body["provisioned_bytes"]
        attachments = body.get("attachments") or []

        if rng.random() < read_ratio:
            choice = rng.random()
            if choice < 0.80:
                recorder.record("list", "read", list_volumes(client))
            else:
                recorder.record("capabilities", "read", capabilities(client))
            continue

        if attachments:
            attachment = attachments[0]
            if attachment["state"] != "active":
                reported = report_active(
                    client,
                    volume_id,
                    attachment["attachment_id"],
                    attachment["attachment_generation"],
                    ids.next("report"),
                )
                recorder.record("report", "write", reported)
            elif grows_left > 0 and rng.random() < 0.08:
                grown = grow_volume(
                    client, volume_id, size + grow_increment, generation, ids.next("grow")
                )
                recorder.record("grow", "write", grown)
                grows_left -= 1
            else:
                detached = detach_volume(
                    client,
                    volume_id,
                    attachment["attachment_id"],
                    attachment["attachment_generation"],
                    ids.next("detach"),
                )
                recorder.record("detach", "write", detached)
        else:
            attach_seq += 1
            attachment_id = f"soak-{volume_id}-{attach_seq}"
            attached = attach_volume(
                client,
                volume_id,
                vm_id,
                attachment_id,
                generation,
                ids.next("attach"),
            )
            recorder.record("attach", "write", attached)


def run_soak(client: HttpClient, args: argparse.Namespace, out_dir: Path) -> dict:
    recorder = Recorder(out_dir, "soak")
    ids = Ids("soak")
    volume_size = args.volume_size
    states = [
        (f"soak-v{worker}", volume_size) for worker in range(args.workers)
    ]

    setup_failures: list[str] = []
    for volume_id, size in states:
        created = create_volume(client, volume_id, size, ids.next("precreate"))
        if not created.ok:
            setup_failures.append(f"{volume_id}: {created.status} {created.error_code}")
    if setup_failures:
        return {
            "scenario": "soak",
            "status": "SETUP_FAILED",
            "setup_failures": setup_failures[:20],
        }

    stop_event = threading.Event()
    sampler = Sampler(
        out_dir / "csv" / "soak-timeline.csv",
        args.journal,
        args.state,
        recorder,
        stop_event,
        pidfile=args.pidfile,
        interval=args.sample_seconds,
    )
    soak_start = time.monotonic()
    stop_ts = soak_start + args.duration_minutes * 60.0
    sampler.start()
    threads = []
    for index, (volume_id, _) in enumerate(states):
        thread = threading.Thread(
            target=soak_worker,
            name=f"soak-{index}",
            args=(
                client,
                recorder,
                ids,
                volume_id,
                stop_ts,
                stop_event,
                args.read_ratio,
                args.grow_increment,
                args.max_extra_grows,
                args.seed + index,
            ),
            daemon=True,
        )
        thread.start()
        threads.append(thread)

    restarts: list[dict] = []
    restart_at = time.monotonic() + args.restart_after_minutes * 60.0
    if args.restart_after_minutes > 0 and restart_at < stop_ts:
        while time.monotonic() < restart_at:
            time.sleep(0.2)
        pid = read_pid(args.pidfile)
        restart_started = now_utc()
        stop_seconds = stop_daemon(pid) if pid else None
        new_pid, _ = start_daemon(args.binary, args.config, args.daemon_log)
        Path(args.pidfile).write_text(f"{new_pid}\n", encoding="ascii")
        healthy_seconds = wait_healthy(args.host, args.port, 120.0)
        restarts.append(
            {
                "at_utc": restart_started,
                "old_pid": pid,
                "new_pid": new_pid,
                "stop_seconds": None if stop_seconds is None else round(stop_seconds, 3),
                "healthy_seconds": round(healthy_seconds, 3),
                "journal_bytes": file_size(args.journal),
                "state_bytes": file_size(args.state),
                "after": replay_snapshot(client, args, [volume_id for volume_id, _ in states]),
            }
        )

    for thread in threads:
        thread.join(timeout=max(1.0, stop_ts - time.monotonic()) + DEFAULT_TIMEOUT)
    elapsed = time.monotonic() - soak_start
    stop_event.set()
    sampler.join(timeout=5)

    cleanup_failures: list[str] = []
    for volume_id, _ in states:
        # Detach any surviving attachment before deleting.
        cleanup_failures.extend(cleanup_volume(client, volume_id, ids))

    summary = recorder.summary()
    summary.update(
        {
            "scenario": "soak",
            "status": "OK",
            "duration_seconds": round(elapsed, 3),
            "workers": args.workers,
            "throughput_ops_per_second": round(recorder.ops / elapsed, 3) if elapsed else None,
            "successful_ops_per_second": (
                round((recorder.ops - recorder.errors) / elapsed, 3) if elapsed else None
            ),
            "read_ratio_target": args.read_ratio,
            "read_ratio_actual": round(recorder.reads / recorder.ops, 4) if recorder.ops else None,
            "resources": resource_summary(sampler),
            "restarts": restarts,
            "cleanup_failures": cleanup_failures[:20],
        }
    )
    return summary


# --------------------------------------------------------------------------
# Scenario: restart replay
# --------------------------------------------------------------------------


def run_restart(client: HttpClient, args: argparse.Namespace) -> dict:
    records = []
    for iteration in range(args.restart_iterations):
        pid = read_pid(args.pidfile)
        before = {
            "journal_bytes": file_size(args.journal),
            "state_bytes": file_size(args.state),
            "pid": pid,
        }
        stop_seconds = stop_daemon(pid) if pid else None
        new_pid, _ = start_daemon(args.binary, args.config, args.daemon_log)
        Path(args.pidfile).write_text(f"{new_pid}\n", encoding="ascii")
        healthy_seconds = wait_healthy(args.host, args.port, 120.0)
        records.append(
            {
                "iteration": iteration + 1,
                "started_utc": now_utc(),
                "before": before,
                "stop_seconds": None if stop_seconds is None else round(stop_seconds, 3),
                "healthy_seconds": round(healthy_seconds, 3),
                "new_pid": new_pid,
                "after": replay_snapshot(client, args),
            }
        )
    label = args.label or "replay"
    return {
        "scenario": f"restart-{label}",
        "status": "OK",
        "iterations": len(records),
        "records": records,
        "healthy_seconds": latency_summary([r["healthy_seconds"] * 1000.0 for r in records]),
    }


# --------------------------------------------------------------------------
# Main
# --------------------------------------------------------------------------


def add_common(parser: argparse.ArgumentParser) -> None:
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--token-file", required=True)
    parser.add_argument("--out-dir", required=True)
    parser.add_argument("--journal", default=None)
    parser.add_argument("--state", default=None)
    parser.add_argument("--repo", default=".")
    parser.add_argument("--seed", type=int, default=43)
    parser.add_argument("--volume-size", type=int, default=16 * 1024 * 1024)
    parser.add_argument("--grow-increment", type=int, default=8 * 1024 * 1024)
    parser.add_argument("--sample-seconds", type=float, default=1.0)
    parser.add_argument("--daemon-pid", type=int, default=None)
    parser.add_argument("--pidfile", default=None)
    parser.add_argument("--binary", default=None)
    parser.add_argument("--config", default=None)
    parser.add_argument("--daemon-log", default=None)
    parser.add_argument("--admission-cap", type=int, default=256)
    parser.add_argument("--queue-timeout", type=float, default=5.0)
    parser.add_argument("--rate-limit", type=int, default=0)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="scenario", required=True)

    micro = sub.add_parser("micro")
    add_common(micro)
    micro.add_argument("--create-delete-iterations", type=int, default=40)
    micro.add_argument("--cycle-iterations", type=int, default=150)
    micro.add_argument("--grow-iterations", type=int, default=30)
    micro.add_argument("--inspect-iterations", type=int, default=400)
    micro.add_argument("--list-iterations", type=int, default=100)
    micro.add_argument("--capabilities-iterations", type=int, default=100)
    micro.add_argument("--scale-volumes", type=int, default=32)

    mixed = sub.add_parser("mixed")
    add_common(mixed)
    mixed.add_argument("--concurrency", type=int, required=True)
    mixed.add_argument("--duration", type=float, default=30.0)
    mixed.add_argument("--read-ratio", type=float, default=0.7)
    mixed.add_argument("--label", default=None, help="unique scenario/volume namespace")

    soak = sub.add_parser("soak")
    add_common(soak)
    soak.add_argument("--duration-minutes", type=float, required=True)
    soak.add_argument("--workers", type=int, default=4)
    soak.add_argument("--read-ratio", type=float, default=0.6)
    soak.add_argument("--restart-after-minutes", type=float, default=0.0)
    soak.add_argument("--max-extra-grows", type=int, default=128)

    restart = sub.add_parser("restart")
    add_common(restart)
    restart.add_argument("--restart-iterations", type=int, default=3)
    restart.add_argument("--label", default=None)

    args = parser.parse_args(argv)
    out_dir = Path(args.out_dir)
    (out_dir / "json").mkdir(parents=True, exist_ok=True)
    (out_dir / "csv").mkdir(parents=True, exist_ok=True)

    token = Path(args.token_file).read_text(encoding="ascii").strip()
    client = HttpClient(args.host, args.port, token)
    sha, dirty = git_identity(args.repo)

    if args.scenario == "micro":
        recorder = Recorder(out_dir, "micro")
        ids = Ids("micro")
        setup = run_micro(client, recorder, ids, args)
        summary = recorder.summary()
        journal_end = file_size(args.journal)
        state_end = file_size(args.state)
        mutations = summary["writes"]
        setup["journal"] = {
            "start_bytes": setup["journal_start_bytes"],
            "end_bytes": journal_end,
            "delta_bytes": (
                None
                if journal_end is None or setup["journal_start_bytes"] is None
                else journal_end - setup["journal_start_bytes"]
            ),
            "mutations": mutations,
            "bytes_per_mutation": (
                None
                if journal_end is None
                or setup["journal_start_bytes"] is None
                or not mutations
                else round((journal_end - setup["journal_start_bytes"]) / mutations, 3)
            ),
            "note": "append-only journal; derived from measured file growth",
        }
        setup["state"] = {
            "start_bytes": setup["state_start_bytes"],
            "end_bytes": state_end,
            "delta_bytes": (
                None
                if state_end is None or setup["state_start_bytes"] is None
                else state_end - setup["state_start_bytes"]
            ),
            "peak_bytes": setup["state_peak_bytes"],
            "samples": setup["state_scale"],
            "note": (
                "whole state file is rewritten and fsynced per mutation; "
                "samples are file size after the create at that volume count; "
                "not separately timed"
            ),
        }
        summary.update(
            {
                "scenario": "micro",
                "status": "OK",
                "setup": setup,
                "volume_size_bytes": args.volume_size,
                "create_delete_iterations": args.create_delete_iterations,
                "cycle_iterations": args.cycle_iterations,
                "grow_iterations": args.grow_iterations,
                "grow_increment_bytes": args.grow_increment,
                "inspect_iterations": args.inspect_iterations,
                "list_iterations": args.list_iterations,
                "capabilities_iterations": args.capabilities_iterations,
            }
        )
    elif args.scenario == "mixed":
        summary = run_mixed(client, args, out_dir)
    elif args.scenario == "soak":
        summary = run_soak(client, args, out_dir)
    elif args.scenario == "restart":
        summary = run_restart(client, args)
    else:
        raise SystemExit(f"unknown scenario {args.scenario}")

    summary.update(
        {
            "generated_utc": now_utc(),
            "source_sha": sha,
            "worktree_dirty": dirty,
            "host": os.uname().nodename,
            "kernel": os.uname().release,
            "python": sys.version.split()[0],
            "argv": sys.argv,
        }
    )
    output = out_dir / "json" / f"{summary['scenario']}.json"
    output.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps({"scenario": summary["scenario"], "status": summary["status"], "json": str(output)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
