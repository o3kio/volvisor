# Campaign flake-rate report

Status: **the calculator exists; no observed flake rate is published and issue
#49's 14-consecutive-night criterion is NOT MET.** The repository contains no
nightly campaign history, so there is nothing to compute the criterion from
yet. This page documents how to collect the artifacts and compute the number
reproducibly when that history exists.

A green sample, a seed corpus and one successful workflow run are **not** a
14-night flake-rate claim and are not production-support evidence.

## What the tool is

[`scripts/flake-rate.py`](../../scripts/flake-rate.py) is a Python 3
standard-library-only reader for the JSONL samples that
[`.github/workflows/campaign-flake.yaml`](../../.github/workflows/campaign-flake.yaml)
writes. Each line is one campaign sample:

```json
{"iteration":1,"result":"pass","started":"2026-10-01T03:17:00Z","finished":"2026-10-01T03:21:00Z","commit":"0123456789abcdef0123456789abcdef01234567"}
```

The tool reports, for the files it is given:

- total samples, passes, failures and `failures/total` (a six-decimal rate
  plus the exact numerator/denominator);
- the distinct UTC dates observed, the date range, and per-date sample and
  failure counts;
- the longest run of **consecutive calendar dates** that each have at least
  one sample and zero failures;
- duplicate `(commit, iteration)` entries; and
- `criterion_met`, which is `true` only when the observed data contains at
  least 14 such consecutive green dates.

It does **not** infer a night that was not sampled, does not treat multiple
samples on one date as multiple nights, does not merge files by commit, and
does not download anything. It also does not prove that a failure was a real
product defect or a harness flake; it only counts the recorded result.

## Collecting the artifacts

The workflow uploads `target/campaign-flake/` as the `campaign-flake`
artifact with 90-day retention. Collect the historical runs before that
window closes, for example with GitHub CLI:

```sh
mkdir -p target/campaign-flake-history
gh run list --workflow campaign-flake.yaml --limit 30
for run_id in $(gh run list --workflow campaign-flake.yaml --limit 30 \
    --json databaseId --jq '.[].databaseId'); do
  gh run download "$run_id" \
    --name campaign-flake \
    --dir "target/campaign-flake-history/$run_id"
done
```

Artifact names are not unique across runs when downloaded into the same
directory, so keep each run in its own directory. Downloaded artifacts are
local working data under the git-ignored `target/` tree; do not commit raw
run directories. If a reviewed evidence bundle is intended, follow
[docs/evidence/README.md](../evidence/README.md) instead of copying an
uncurated directory.

## Exact command

Assess the current run only (as the workflow summary does):

```sh
python3 scripts/flake-rate.py target/campaign-flake/iterations.jsonl
```

Assess the downloaded history (the command that can satisfy the criterion):

```sh
python3 scripts/flake-rate.py \
  target/campaign-flake-history/*/iterations.jsonl
```

The shell expands the glob; the script also expands quoted globs itself.
`--json` emits the same report as a machine-readable object. `--allow-empty`
turns an empty input into an explicit success while still reporting
`criterion_met: false`; without it, empty input is an invalid-input error.

Exit codes:

| Code | Meaning |
|---|---|
| `0` | Valid input; report emitted (even when `criterion_met` is false). |
| `1` | Invalid or empty input: malformed JSON, unknown result, missing field, non-UTC/invalid timestamp, unreadable file or no samples. |
| `2` | Usage error (for example, no input files). |

## `criterion_met` semantics

Issue #49 asks for **no open campaign flake for 14 consecutive nightly
runs**. The tool maps that to:

1. Group samples by the UTC calendar date of their `started` timestamp.
2. A date qualifies only if it has at least one sample and every sample on
   that date is `pass`.
3. Sort the distinct dates and find the longest run where each date is
   exactly one calendar day after the previous qualifying date.
4. `criterion_met` is true only when that run is at least 14 dates long.

Consequences:

- A missing night breaks the run; the tool does not fill gaps.
- Two samples on the same date still count as one date.
- A failure on a date disqualifies that date and breaks the run.
- An earlier historical failure does not prevent a later 14-date green run
  from satisfying the criterion; the criterion is about a consecutive green
  window, not a lifetime zero-failure rate.
- A `true` value over fixtures or a single run is a property of that input,
  not evidence that the campaign ran for 14 nights.

## Worked example from the fixtures

The committed, synthetic fixtures exercise the semantics; they are **not
campaign evidence**.

Three green nights:

```console
$ python3 scripts/flake-rate.py scripts/tests/fixtures/all-green-3-nights.jsonl
flake-rate report
  files: 1
  samples: 3
  passes: 3
  failures: 0
  flake_rate: 0.000000 (0/3)
  date range: 2026-10-01 .. 2026-10-03 (3 distinct UTC dates)
  per-date samples/failures:
    2026-10-01: samples=1 failures=0
    2026-10-02: samples=1 failures=0
    2026-10-03: samples=1 failures=0
  longest consecutive green dates: 3
  required consecutive green dates: 14
  duplicate (commit, iteration) entries: none
  criterion_met: false
  note: absent nights are not inferred; the criterion covers only the supplied samples. One workflow run is not 14 nights.
```

Fourteen consecutive green dates:

```console
$ python3 scripts/flake-rate.py scripts/tests/fixtures/fourteen-green-nights.jsonl
...
  longest consecutive green dates: 14
  required consecutive green dates: 14
  criterion_met: true
```

The count is the input's shape, not a claim about the repository. The
`failure-breaks-run` fixture shows one failed date splitting sixteen dates
into a seven-date run before the failure and an eight-date run after it, so
the reported longest run is eight (`criterion_met: false`), and
`multiple-samples-same-date` shows nine passes across three dates producing a
three-date run, not nine nights. `scripts/tests/test_flake_rate.py` asserts
these and the invalid-input behavior.

## Current honest status

- **Observed flake rate: none published.** There is no in-repo history of
  nightly `campaign-flake` artifacts to compute one from.
- **14-consecutive-night criterion: NOT MET.** Existing workflows collect
  samples but no nightly history exists in the repository, and the workflow
  summary reports only the current run.
- The calculator exists so that, once enough historical artifacts are
  downloaded, the rate and criterion can be computed reproducibly and
  reviewed instead of asserted.
- Do not publish a flake rate from a single green run, from the fuzz corpus,
  or from fixtures. A rate must name the exact artifact set, the date range,
  the commit(s), and the command used.
