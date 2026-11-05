# NON-EVIDENCE DIAGNOSTIC — c128 run-to-run variance

**This directory is not measured evidence.** It is a four-run `--skip-soak`
diagnostic executed *after* the measured run, solely to test whether the
measured c128 throughput drop (`610.988 → 462.899` ops/s) can be attributed
to the measured change. It was not used to select, replace or support any
claim, and no number here is a measured result for `7204a53`. The measured
evidence is the bundle root's `json/`, `csv/`, `summary/`, `records/`,
`raw/`, `ENVIRONMENT.txt` and `comparison/`.

## Design

Two binaries alternated with `--skip-soak` (no soak; the micro run, the same
30 s 1/8/32/128 tiers, the probes and the pre-soak restart still ran):

- **A** = `7204a53` — byte-identical to the measured binary,
  SHA256 `db09dff8090eae07a8ea26c70bb3bfcf8200969b82b49d165f72a801eb6a8717`.
- **B** = `2c5b065` — rebuilt at that source in a detached worktree,
  SHA256 `58b43d5ed6306924817706a15d5778cb6ff2f672883bd508980196fad17d27e9`.
  The build bytes differ from the published E3 build at the same source;
  the source SHA is identical.

Order: A1, B1, A2, B2. Host, harness scripts, lab construction and tier
parameters are the same as the measured run. Each source run's own
`MANIFEST.sha256` verified before any record was copied.

**Dirty-flag disclosure.** The A runs record `worktree_dirty: yes` because
the untracked published-bundle directory existed while they ran. The flag is
a metadata artifact of untracked docs; the executed binary is byte-identical
(SHA256 `db09…`) to the clean measured run, and no `crates/` or `scripts/`
file differed. The B runs are clean. These runs support no clean-SHA claim
regardless; they are non-evidence.

## Result

| run | arm | c128 ops/s | c128 p95 ms | c128 attach p50 ms | worktree dirty |
|---|---|---|---|---|---|
| A1 | 7204a53 | 626.011 | 89.383 | 10.942 | yes (untracked docs only) |
| B1 | 2c5b065 | 432.752 | 137.346 | 25.506 | no |
| A2 | 7204a53 | 439.497 | 135.620 | 25.275 | yes (untracked docs only) |
| B2 | 2c5b065 | 427.457 | 141.587 | 24.676 | no |

The c128 tier swings by about 31% between adjacent runs (A1 626.011 →
B1 432.752; equivalently B1 is about 45% below A1), and the arms are within
1.6% at their closest (B1 432.752 vs A2 439.497) but cannot be separated at
n=2; B2 is 427.457 and A1 is a high-state outlier. The later runs are all in a low state, with attach p50 ≈
25 ms versus A1's 10.9 ms. The measured run (462.899) and its earlier
same-SHA `--skip-soak` repeat (519.539, published as non-evidence under
[../same-sha-repeat/](../same-sha-repeat/README.md)) fall inside the observed
range.

The diagnostic therefore shows that the c128 tier is dominated by
time-varying host state in this lab, not by the binary: the measured c128
delta cannot be attributed to the change from `2c5b065` to `7204a53`. It
does **not** prove the absence of a small effect; it shows that a single
c128 sample cannot establish one in either direction.

## Files

- `runs.csv` — one row per run, values read from the copied aggregate JSON.
- `<run>/ENVIRONMENT.txt` and `<run>/json/mixed-c{1,8,32,128}.json` — the
  exact records used.

Daemon logs and full run trees are not included. This directory contains no
micro, soak, probe or restart record; only the mixed-c aggregates were
copied from the four `--skip-soak` runs.
