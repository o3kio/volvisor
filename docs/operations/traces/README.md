# Trace evidence for issue #44

## Attach trace

`attach.jsonl` is the committed example for one create followed by one
attach. It was generated from the API code at commit
`d164495c623bfb15bd6c7f17c2c71a1b49930222` on 2026-11-03 with:

```text
cargo test -p volvisor-api --test trace_attach -- --ignored --nocapture \
  2>&1 | grep '^{' > docs/operations/traces/attach.jsonl
```

The generator drives the **real router** with a real on-disk journal and the
fake provider, using the same JSON tracing subscriber shape as the daemon.
The trace therefore shows the real `operation` → `provider_call` span nesting
and the audit event; provider-specific work is fake by construction.

Each line is one JSON event. Both the operation span and the provider span
carry the caller's `operation_id`, so the two requests can be followed end to
end:

```text
jq -c 'select(.span.name == "operation") |
  {ts: .timestamp, kind: .span.kind, operation_id: .span.operation_id,
   message: .fields.message}' docs/operations/traces/attach.jsonl
```

The test's admin token is a hard-coded test credential and is **not** present
in the captured lines. No volume payloads, principals other than the test
principal, or secrets are emitted by these events.

Regenerating this file is an explicit evidence step: the generator test is
`#[ignore]`d so the ordinary workspace run does not rewrite committed
evidence.

## Validating committed traces

Two independent checks validate every committed `*.jsonl` file. They are
required because a trace that is malformed, uncorrelated or leaks a credential
is not evidence.

The ordinary integration test runs in the normal workspace suite (it is not
`#[ignore]`d):

```text
cargo test -p volvisor-api --test trace_evidence --locked
```

The operator/CI script performs the same structural, correlation and
secret-field checks and can be run on one file or all of them:

```text
python3 scripts/validate-traces.py docs/operations/traces/attach.jsonl
python3 scripts/validate-traces.py docs/operations/traces/*.jsonl
```

The script exits `0` for valid input, `1` for an invalid trace, and `2` for a
usage error; `--help` documents `--require-operation-id`, which additionally
requires a non-empty `operation_id` on every span. It uses only the Python 3
standard library.

Both checks require one JSON object per non-empty line, an RFC 3339
`timestamp`, `level` and `fields.message`, and span names from the bounded set
documented in [`../observability.md`](../observability.md). `operation` and
`provider_call` spans must carry a non-empty `operation_id`, and a
`provider_call` must include its enclosing `operation` in `spans` with the same
id. For `attach.jsonl` the create and attach operation ids must both appear,
and the audit events for `/v2/volumes` and
`/v2/volumes/{volume_id}/attach` must carry those matching ids. No line may
contain the `trace-admin-token` test value or a secret-like field such as
`authorization`, `token`, `secret` or `password`.

The test and script discover files by name. If a future `migration.jsonl` is
committed it is validated automatically; neither check requires it to exist.

## Migration trace: NOT RUN

`migration.jsonl` is **not committed** because this workspace has no real
migration rig. The in-repo `migration_e2e` test uses two simulated DRBD
worlds, a loopback witness and two `FakeVmm` instances; it exercises the real
peer HTTP path but it is not a cross-host DRBD/VMM migration. Presenting its
output as a real migration trace would overstate the evidence
(AGENTS rule 12).

The trace requires all of the following:

- two hosts with the DRBD 9 kernel module and `drbd-utils`, plus a backing
  LV/device on each (`docs/testing/real-backends.md` records DRBD as blocked
  here: no module, no utilities, no second peer host);
- a real witness endpoint reachable by both hosts, with the per-host W8
  credentials and a configured `witness_url`;
- a real `VmmController` (not `FakeVmm`) and a VM with an attached volume
  eligible for whole-VM mobility;
- the daemon peer credential and a configured pair of daemons.

Once that rig exists, run the daemons with the JSON tracing subscriber,
drive the real `POST /v2/migrations` prepare/transfer path with a stable
`operation_id`, and capture only the JSON lines:

```text
RUST_LOG=info volvisord --config /etc/volvisor/volvisord.toml 2>&1 \
  | grep '^{' > docs/operations/traces/migration.jsonl
```

Do not check a simulated trace in as a real one, and do not mark the issue's
migration-trace box done until the output comes from that rig. **No simulated
or synthesized `migration.jsonl` is committed**, and the in-repo simulated
migration test output is not evidence for this file.
