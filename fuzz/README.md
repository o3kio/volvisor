# Fuzz targets (issue #49)

This directory is a **standalone Cargo workspace** (`fuzz/Cargo.toml`).
Its `libfuzzer-sys` dependency (only the sanitized campaigns need
nightly) never enters the main workspace lockfile, and
`cargo test --workspace --locked` never builds it.

## Targets

| Target | Input | Invariant |
|---|---|---|
| `journal_frame` | Raw journal bytes | The frame-chain walker and the production streaming `read_frame` chain agree frame-for-frame; neither panics or loops, both stop at a torn/corrupt boundary, and neither consumes past the input. |
| `journal_record` | One frame payload (JSON record envelope) | The same version probe and strict `Envelope` decode replay uses: supported versions decode, unknown fields and unsupported versions are refused. |
| `lvm_state` | One LVM state file | Both direct serde decode and the production `LvmState::load` path terminate without panic; a newer schema is a typed refusal. |
| `request_validation` | One public request envelope | Parsing, `validate()` and `request_hash()` never panic or loop on arbitrary bytes. |

## Seed corpus

`corpus/<target>/` holds committed seeds generated from the current
schema and the N-1 fixtures. File names are the regression contract:

- `valid-*` inputs must decode (or load/validate) successfully;
- `invalid-*` inputs must be refused without panicking.

The corpus is also exercised deterministically on every change by the
`fuzz_corpus` integration tests in `volvisor-journal`, `volvisor-lvm`
and `volvisor-types`, so the committed seeds do not depend on the
nightly workflow running. When a serialized shape changes, update the
seeds and the N-1 fixture together; see
[`docs/schema-compatibility.md`](../docs/schema-compatibility.md).

### Corpus hygiene

`cargo fuzz` writes newly discovered coverage inputs into the same
`corpus/<target>/` directory under hash-like names (for example
`1a2b3c...`). Those scratch inputs are **not** part of the regression
contract: the `fuzz_corpus` tests ignore anything that does not start
with `valid-` or `invalid-`. After a local campaign:

1. Triage each hash-named input. A new crash or a genuinely new
   coverage shape is worth keeping: rename it to `valid-<what>.bin`
   (or `invalid-<what>.bin`) with a description that matches its
   invariant, and commit it.
2. Delete the remaining scratch inputs before committing so the
   reviewed corpus stays small and every file has an explicit
   expectation.

The nightly workflow uploads the evolved corpus as an artifact; it
does not commit it back.

## Running

```sh
cd fuzz
cargo +nightly fuzz run journal_frame
cargo +nightly fuzz run journal_record
cargo +nightly fuzz run lvm_state
cargo +nightly fuzz run request_validation
```

The workspace `rust-toolchain.toml` pins stable, so plain `cargo fuzz`
would pick stable and fail on `-Zsanitizer`. Use `+nightly` locally; the
nightly workflow sets `RUSTUP_TOOLCHAIN=nightly` for the same reason.

`.github/workflows/fuzz.yaml` runs each target with a bounded budget
nightly and on manual dispatch. A crash is uploaded as an artifact.
The workflow is not a required pull-request gate.

## What this is not

A bounded fuzz campaign is **not** a 14-night flake-rate claim, a
coverage measurement, or production-support evidence. Issue #49's
14-consecutive-night criterion remains open until the runs exist and
are recorded.
