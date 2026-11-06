//! In-process Prometheus metrics, hand-formatted (no prometheus crate).
//!
//! The families exposed on `/metrics` in the Prometheus text exposition
//! format are:
//!
//! - `http_requests_total{route,code}`: HTTP requests by matched route
//!   pattern and response status code (counter);
//! - `http_request_duration_seconds{route,class}`: HTTP request latency by
//!   matched route pattern and fixed status class (`1xx` ... `5xx`)
//!   (histogram);
//! - `http_requests_in_flight`: requests currently being served (gauge);
//! - `operations_total{kind,outcome}`: volume operations by kind
//!   (`create_volume`, ...) and outcome (`success`, `failure`, `replayed`,
//!   `in_doubt`, `conflict`, `resolved`, `re_drive`, `expired`,
//!   `journal_full`) (counter);
//! - `provider_operation_duration_seconds{kind,outcome}`: the provider call
//!   latency of operations that actually executed, by operation kind and the
//!   fixed outcome (`success` or `failure`) (histogram);
//! - `journal_append_failures_total{stage,kind}`: journal append failures by
//!   stage (`intent`, `outcome`) and operation kind (counter);
//! - `journal_records`, `journal_log_bytes`, `journal_open`, and the ADR-0014
//!   bounds (`journal_max_log_bytes`, `journal_hard_ceiling_bytes`,
//!   `journal_in_flight_operations`, `journal_terminal_operations`,
//!   `journal_tombstones`, `journal_tombstone_capacity`,
//!   `journal_cannot_fit`): sampled from the live journal (gauges/counters);
//! - `journal_refusals_total{reason}`: durable appends refused by bounded
//!   admission (`ceiling` or `cannot_fit`) (counter);
//! - `reconcile_age_seconds{scope}`: seconds since the last completed
//!   reconcile pass (`startup`, and the `grow`/`move` background passes on
//!   a daemon that runs them) (gauge);
//! - `witness_reachable`: `1`/`0` from the last transport-reachability probe
//!   of the configured witness (gauge; absent when no probe is configured).
//!
//! Counters and histograms are [`AtomicU64`]s held in maps guarded by a
//! short-lived [`std::sync::Mutex`]; nothing in this module ever awaits while
//! holding a lock.
//!
//! ## Cardinality bounds
//!
//! - HTTP labels are the axum matched route **pattern** (a fixed route table
//!   of roughly 30 patterns) or the fixed `<unmatched>` value. Raw paths and
//!   path parameters (volume, attachment, device, migration and VM ids) are
//!   never labels.
//! - The status label of the latency histogram is one of five fixed classes;
//!   the request counter keeps the exact numeric status code, which is a
//!   bounded set for this API.
//! - Provider operation kinds and outcomes are compile-time constants.
//! - Journal failure stages are the fixed `intent`/`outcome` values; reconcile
//!   scopes are the fixed `startup`/`grow`/`move` values.
//! - No label carries a principal, token, URL, error string or free-form
//!   detail.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Histogram bucket upper bounds in seconds (the same fixed set for HTTP and
/// provider latency).
const DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// A live journal sample rendered by `/metrics`.
#[derive(Clone, Copy, Debug)]
pub struct JournalSnapshot {
    /// Valid records replayed plus records appended since open, when the
    /// journal is readable.
    pub records: Option<u64>,
    /// Size of the journal log file in bytes, when it can be stat'ed.
    pub log_bytes: Option<u64>,
    /// Successful compactions since open.
    pub compactions: Option<u64>,
    /// Failed compaction attempts since open. A persistently non-zero value
    /// means the log is not being bounded.
    pub compaction_failures: Option<u64>,
    /// Terminal plus in-flight operations currently retained.
    pub retained_operations: Option<u64>,
    /// In-flight intents (no outcome yet); never evicted.
    pub in_flight_operations: Option<u64>,
    /// Terminal operations currently tracked in terminal order.
    pub terminal_operations: Option<u64>,
    /// Exact tombstone fingerprints currently retained.
    pub tombstones: Option<u64>,
    /// Configured tombstone window bound.
    pub tombstone_capacity: Option<u64>,
    /// Configured compaction byte trigger; `None`/0 means disabled.
    pub max_log_bytes: Option<u64>,
    /// Configured hard ceiling above which appends are refused.
    pub hard_ceiling_bytes: Option<u64>,
    /// Compaction attempts since open (successes plus failures).
    pub compaction_attempts: Option<u64>,
    /// Durable appends refused at the hard ceiling since open.
    pub ceiling_refusals: Option<u64>,
    /// New intents refused because the in-flight set cannot fit one frame.
    pub cannot_fit_refusals: Option<u64>,
    /// Whether new-intent admission is currently closed by cannot-fit.
    pub cannot_fit: Option<bool>,
    /// Whether the daemon still holds an open journal.
    pub open: bool,
}

/// A fixed-bucket duration histogram backed by atomics.
#[derive(Debug)]
struct Histogram {
    /// One counter per finite bucket, plus a final `+Inf` overflow bucket.
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    /// Sum in nanoseconds (`Duration` keeps the conversion lossless).
    sum_nanos: AtomicU64,
}

impl Histogram {
    fn new() -> Self {
        Self {
            buckets: (0..=DURATION_BUCKETS.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            count: AtomicU64::new(0),
            sum_nanos: AtomicU64::new(0),
        }
    }

    fn observe(&self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        let index = DURATION_BUCKETS
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(DURATION_BUCKETS.len());
        if let Some(bucket) = self.buckets.get(index) {
            bucket.fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, labels: &str) {
        let mut cumulative = 0_u64;
        for (index, bound) in DURATION_BUCKETS.iter().enumerate() {
            let value = self
                .buckets
                .get(index)
                .map_or(0, |bucket| bucket.load(Ordering::Relaxed));
            cumulative = cumulative.saturating_add(value);
            write_sample(
                out,
                name,
                "_bucket",
                labels,
                &format!("le=\"{bound}\""),
                &cumulative.to_string(),
            );
        }
        cumulative = cumulative.saturating_add(
            self.buckets
                .get(DURATION_BUCKETS.len())
                .map_or(0, |bucket| bucket.load(Ordering::Relaxed)),
        );
        write_sample(
            out,
            name,
            "_bucket",
            labels,
            "le=\"+Inf\"",
            &cumulative.to_string(),
        );
        let sum = Duration::from_nanos(self.sum_nanos.load(Ordering::Relaxed)).as_secs_f64();
        write_sample(out, name, "_sum", labels, "", &format!("{sum}"));
        write_sample(
            out,
            name,
            "_count",
            labels,
            "",
            &self.count.load(Ordering::Relaxed).to_string(),
        );
    }
}

/// Prometheus counters, histograms and gauges for one API server instance.
#[derive(Debug)]
pub struct Metrics {
    /// `http_requests_total` labels: (route pattern, status code).
    http_requests: Mutex<BTreeMap<(String, u16), AtomicU64>>,
    /// `http_request_duration_seconds` labels: (route pattern, status class).
    http_duration: Mutex<BTreeMap<(String, &'static str), Histogram>>,
    /// `operations_total` labels: (operation kind, outcome).
    operations: Mutex<BTreeMap<(&'static str, &'static str), AtomicU64>>,
    /// `provider_operation_duration_seconds` labels: (operation kind, outcome).
    provider_duration: Mutex<BTreeMap<(&'static str, &'static str), Histogram>>,
    /// `journal_append_failures_total` labels: (stage, operation kind).
    journal_append_failures: Mutex<BTreeMap<(&'static str, &'static str), AtomicU64>>,
    /// `http_requests_in_flight` — all HTTP requests, including probes.
    http_in_flight: AtomicU64,
    /// `witness_reachable`: `-1` unknown/not probed, `0` down, `1` up.
    witness_reachable: AtomicI8,
    /// Last completed reconcile pass per fixed scope, as Unix seconds.
    reconcile_completed_at: Mutex<BTreeMap<&'static str, u64>>,
}

impl Metrics {
    /// A fresh, empty set of metrics.
    #[must_use]
    pub fn new() -> Self {
        Self {
            http_requests: Mutex::new(BTreeMap::new()),
            http_duration: Mutex::new(BTreeMap::new()),
            operations: Mutex::new(BTreeMap::new()),
            provider_duration: Mutex::new(BTreeMap::new()),
            journal_append_failures: Mutex::new(BTreeMap::new()),
            http_in_flight: AtomicU64::new(0),
            witness_reachable: AtomicI8::new(-1),
            reconcile_completed_at: Mutex::new(BTreeMap::new()),
        }
    }

    /// Count one served HTTP request and observe its latency.
    pub(crate) fn record_http(&self, route: &str, code: u16, duration: Duration) {
        let mut counters = self.lock_http();
        counters
            .entry((route.to_owned(), code))
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
        drop(counters);

        let class = status_class(code);
        let mut histograms = self.lock_http_duration();
        histograms
            .entry((route.to_owned(), class))
            .or_insert_with(Histogram::new)
            .observe(duration);
    }

    /// One request entered the HTTP surface.
    pub(crate) fn request_started(&self) {
        self.http_in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// One request left the HTTP surface.
    pub(crate) fn request_finished(&self) {
        // Saturating decrement: a bug must never wrap the gauge to a huge
        // value, and the guard is the only caller. A compare-exchange loop
        // keeps MSRV 1.85 compatibility (newer `fetch_update` spellings are
        // not available there).
        let mut current = self.http_in_flight.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_sub(1);
            match self.http_in_flight.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    /// Count one resolved volume operation.
    pub(crate) fn record_operation(&self, kind: &'static str, outcome: &'static str) {
        let mut counters = self.lock_operations();
        counters
            .entry((kind, outcome))
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Observe one executed provider call's latency.
    pub(crate) fn record_provider_operation(
        &self,
        kind: &'static str,
        outcome: &'static str,
        duration: Duration,
    ) {
        let mut histograms = self.lock_provider_duration();
        histograms
            .entry((kind, outcome))
            .or_insert_with(Histogram::new)
            .observe(duration);
    }

    /// Count one failed journal append.
    pub(crate) fn record_journal_append_failure(&self, stage: &'static str, kind: &'static str) {
        let mut counters = self.lock_journal_append_failures();
        counters
            .entry((stage, kind))
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record a successful reconcile/startup pass for a fixed scope. Public
    /// so the daemon's background reconcile tasks can report their own real
    /// passes; callers must pass one of the fixed scopes.
    pub fn record_reconcile_complete(&self, scope: &'static str) {
        if let Ok(mut passes) = self.reconcile_completed_at.lock() {
            passes.insert(scope, unix_now_secs());
        }
    }

    /// Update the witness reachability gauge (`None` = not probed).
    pub(crate) fn set_witness_reachable(&self, reachable: Option<bool>) {
        self.witness_reachable.store(
            match reachable {
                None => -1,
                Some(false) => 0,
                Some(true) => 1,
            },
            Ordering::Relaxed,
        );
    }

    /// Render the metrics in the Prometheus text exposition format
    /// (labels in alphabetical order, output deterministic).
    #[must_use]
    pub fn render(&self) -> String {
        self.render_with_journal(None)
    }

    /// Render the metrics and, when supplied, the live journal gauges.
    #[must_use]
    pub fn render_with_journal(&self, journal: Option<JournalSnapshot>) -> String {
        let mut out = String::new();
        self.render_http(&mut out);
        self.render_operations(&mut out);
        self.render_provider(&mut out);
        self.render_journal_failures(&mut out);
        if let Some(journal) = journal {
            render_journal(&mut out, journal);
        }
        self.render_reconcile(&mut out);
        self.render_witness(&mut out);
        out
    }

    fn render_http(&self, out: &mut String) {
        out.push_str(
            "# HELP http_requests_total HTTP requests served, by route and response code.\n",
        );
        out.push_str("# TYPE http_requests_total counter\n");
        for ((route, code), counter) in &*self.lock_http() {
            let _ = writeln!(
                out,
                "http_requests_total{{code=\"{code}\",route=\"{}\"}} {}",
                escape_label_value(route),
                counter.load(Ordering::Relaxed),
            );
        }

        out.push_str(
            "# HELP http_request_duration_seconds HTTP request latency, by route and \
             status class.\n",
        );
        out.push_str("# TYPE http_request_duration_seconds histogram\n");
        for ((route, class), histogram) in &*self.lock_http_duration() {
            let labels = format!("class=\"{class}\",route=\"{}\"", escape_label_value(route));
            histogram.render(out, "http_request_duration_seconds", &labels);
        }

        out.push_str("# HELP http_requests_in_flight HTTP requests currently being served.\n");
        out.push_str("# TYPE http_requests_in_flight gauge\n");
        let _ = writeln!(
            out,
            "http_requests_in_flight {}",
            self.http_in_flight.load(Ordering::Relaxed)
        );
    }

    fn render_operations(&self, out: &mut String) {
        out.push_str("# HELP operations_total Volume operations resolved, by kind and outcome.\n");
        out.push_str("# TYPE operations_total counter\n");
        for ((kind, outcome), counter) in &*self.lock_operations() {
            let _ = writeln!(
                out,
                "operations_total{{kind=\"{}\",outcome=\"{}\"}} {}",
                escape_label_value(kind),
                escape_label_value(outcome),
                counter.load(Ordering::Relaxed),
            );
        }
    }

    fn render_provider(&self, out: &mut String) {
        out.push_str(
            "# HELP provider_operation_duration_seconds Provider call latency of executed \
             operations, by kind and outcome.\n",
        );
        out.push_str("# TYPE provider_operation_duration_seconds histogram\n");
        for ((kind, outcome), histogram) in &*self.lock_provider_duration() {
            let labels = format!(
                "kind=\"{}\",outcome=\"{}\"",
                escape_label_value(kind),
                escape_label_value(outcome)
            );
            histogram.render(out, "provider_operation_duration_seconds", &labels);
        }
    }

    fn render_journal_failures(&self, out: &mut String) {
        out.push_str(
            "# HELP journal_append_failures_total Journal appends that failed after the \
             operation reached the journal pipeline.\n",
        );
        out.push_str("# TYPE journal_append_failures_total counter\n");
        for ((stage, kind), counter) in &*self.lock_journal_append_failures() {
            let _ = writeln!(
                out,
                "journal_append_failures_total{{kind=\"{}\",stage=\"{}\"}} {}",
                escape_label_value(kind),
                escape_label_value(stage),
                counter.load(Ordering::Relaxed),
            );
        }
    }

    fn render_reconcile(&self, out: &mut String) {
        let now = unix_now_secs();
        let passes = self
            .reconcile_completed_at
            .lock()
            .map(|passes| passes.clone())
            .unwrap_or_default();
        if passes.is_empty() {
            return;
        }
        out.push_str(
            "# HELP reconcile_age_seconds Seconds since the last completed reconcile pass for a \
             scope, or since its task started if no pass has completed.\n",
        );
        out.push_str("# TYPE reconcile_age_seconds gauge\n");
        for (scope, completed_at) in passes {
            let age = now.saturating_sub(completed_at);
            let _ = writeln!(out, "reconcile_age_seconds{{scope=\"{scope}\"}} {age}");
        }
    }

    fn render_witness(&self, out: &mut String) {
        let witness = self.witness_reachable.load(Ordering::Relaxed);
        if witness < 0 {
            return;
        }
        out.push_str(
            "# HELP witness_reachable Last transport-reachability probe of the configured \
             witness (1 reachable, 0 unreachable).\n",
        );
        out.push_str("# TYPE witness_reachable gauge\n");
        let _ = writeln!(out, "witness_reachable {witness}");
    }

    fn lock_http(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, u16), AtomicU64>> {
        // Counters are advisory: a poisoned lock (a panic while counting)
        // must not take the HTTP surface down with it.
        self.http_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_http_duration(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(String, &'static str), Histogram>> {
        self.http_duration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_operations(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(&'static str, &'static str), AtomicU64>> {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_provider_duration(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(&'static str, &'static str), Histogram>> {
        self.provider_duration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_journal_append_failures(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<(&'static str, &'static str), AtomicU64>> {
        self.journal_append_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the live journal gauges, omitting unavailable samples rather than
/// fabricating zeroes.
fn render_journal(out: &mut String, journal: JournalSnapshot) {
    out.push_str("# HELP journal_open Whether the daemon holds an open intent journal.\n");
    out.push_str("# TYPE journal_open gauge\n");
    let _ = writeln!(out, "journal_open {}", u8::from(journal.open));
    if let Some(records) = journal.records {
        out.push_str("# HELP journal_records Valid journal records (replayed plus appended).\n");
        out.push_str("# TYPE journal_records gauge\n");
        let _ = writeln!(out, "journal_records {records}");
    }
    if let Some(bytes) = journal.log_bytes {
        out.push_str("# HELP journal_log_bytes Size of the journal log file in bytes.\n");
        out.push_str("# TYPE journal_log_bytes gauge\n");
        let _ = writeln!(out, "journal_log_bytes {bytes}");
    }
    if let Some(compactions) = journal.compactions {
        out.push_str(
            "# HELP journal_compactions_total Successful journal compactions since open.\n",
        );
        out.push_str("# TYPE journal_compactions_total counter\n");
        let _ = writeln!(out, "journal_compactions_total {compactions}");
    }
    if let Some(failures) = journal.compaction_failures {
        out.push_str(
            "# HELP journal_compaction_failures_total Failed journal compaction attempts \
             since open; a sustained increase means the log is not being bounded.\n",
        );
        out.push_str("# TYPE journal_compaction_failures_total counter\n");
        let _ = writeln!(out, "journal_compaction_failures_total {failures}");
    }
    if let Some(retained) = journal.retained_operations {
        out.push_str(
            "# HELP journal_retained_operations Terminal plus in-flight operations retained \
             in the idempotency registry.\n",
        );
        out.push_str("# TYPE journal_retained_operations gauge\n");
        let _ = writeln!(out, "journal_retained_operations {retained}");
    }
    if let Some(in_flight) = journal.in_flight_operations {
        out.push_str(
            "# HELP journal_in_flight_operations In-flight intents (no outcome yet); these \
             are never evicted.\n",
        );
        out.push_str("# TYPE journal_in_flight_operations gauge\n");
        let _ = writeln!(out, "journal_in_flight_operations {in_flight}");
    }
    if let Some(terminal) = journal.terminal_operations {
        out.push_str(
            "# HELP journal_terminal_operations Terminal operations currently tracked in \
             terminal order.\n",
        );
        out.push_str("# TYPE journal_terminal_operations gauge\n");
        let _ = writeln!(out, "journal_terminal_operations {terminal}");
    }
    if let Some(tombstones) = journal.tombstones {
        out.push_str(
            "# HELP journal_tombstones Exact evicted-operation fingerprints in the tombstone \
             ring.\n",
        );
        out.push_str("# TYPE journal_tombstones gauge\n");
        let _ = writeln!(out, "journal_tombstones {tombstones}");
    }
    if let Some(capacity) = journal.tombstone_capacity {
        out.push_str("# HELP journal_tombstone_capacity Configured tombstone ring bound.\n");
        out.push_str("# TYPE journal_tombstone_capacity gauge\n");
        let _ = writeln!(out, "journal_tombstone_capacity {capacity}");
    }
    render_journal_bounds(out, &journal);
}

/// Render the ADR-0014 bounds and refusal counters.
fn render_journal_bounds(out: &mut String, journal: &JournalSnapshot) {
    if let Some(trigger) = journal.max_log_bytes {
        out.push_str(
            "# HELP journal_max_log_bytes Configured compaction byte trigger; 0 disables the \
             byte trigger.\n",
        );
        out.push_str("# TYPE journal_max_log_bytes gauge\n");
        let _ = writeln!(out, "journal_max_log_bytes {trigger}");
    }
    if let Some(ceiling) = journal.hard_ceiling_bytes {
        out.push_str(
            "# HELP journal_hard_ceiling_bytes Configured hard ceiling above which durable \
             appends are refused.\n",
        );
        out.push_str("# TYPE journal_hard_ceiling_bytes gauge\n");
        let _ = writeln!(out, "journal_hard_ceiling_bytes {ceiling}");
    }
    if let Some(attempts) = journal.compaction_attempts {
        out.push_str(
            "# HELP journal_compaction_attempts_total Journal compaction attempts since open \
             (successes plus failures).\n",
        );
        out.push_str("# TYPE journal_compaction_attempts_total counter\n");
        let _ = writeln!(out, "journal_compaction_attempts_total {attempts}");
    }
    if let Some(cannot_fit) = journal.cannot_fit {
        out.push_str(
            "# HELP journal_cannot_fit Whether new-intent admission is closed because the \
             in-flight set cannot fit one snapshot frame.\n",
        );
        out.push_str("# TYPE journal_cannot_fit gauge\n");
        let _ = writeln!(out, "journal_cannot_fit {}", u8::from(cannot_fit));
    }
    if journal.ceiling_refusals.is_some() || journal.cannot_fit_refusals.is_some() {
        out.push_str(
            "# HELP journal_refusals_total Durable appends refused by bounded admission, by \
             reason (ceiling or cannot_fit).\n",
        );
        out.push_str("# TYPE journal_refusals_total counter\n");
        if let Some(ceiling) = journal.ceiling_refusals {
            let _ = writeln!(
                out,
                "journal_refusals_total{{reason=\"ceiling\"}} {ceiling}"
            );
        }
        if let Some(cannot_fit) = journal.cannot_fit_refusals {
            let _ = writeln!(
                out,
                "journal_refusals_total{{reason=\"cannot_fit\"}} {cannot_fit}"
            );
        }
    }
}

/// Fixed status-class label for a response code.
fn status_class(code: u16) -> &'static str {
    match code {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        // 500..=599, plus anything else `http::StatusCode` can construct
        // (100..=999): out-of-class codes group with the server-error class
        // rather than being silently dropped.
        _ => "5xx",
    }
}

/// Write one labelled or unlabelled sample line.
fn write_sample(
    out: &mut String,
    name: &str,
    suffix: &str,
    labels: &str,
    extra: &str,
    value: &str,
) {
    if labels.is_empty() && extra.is_empty() {
        let _ = writeln!(out, "{name}{suffix} {value}");
        return;
    }
    let mut rendered = String::from(labels);
    if !extra.is_empty() {
        if !rendered.is_empty() {
            rendered.push(',');
        }
        rendered.push_str(extra);
    }
    let _ = writeln!(out, "{name}{suffix}{{{rendered}}} {value}");
}

/// Escape a label value for the Prometheus text format.
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Current Unix time in seconds (`0` before the epoch).
fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_render_cumulative_and_bounded() {
        let metrics = Metrics::new();
        metrics.record_http("/v2/volumes/{volume_id}", 200, Duration::from_millis(3));
        metrics.record_http("/v2/volumes/{volume_id}", 200, Duration::from_millis(200));
        metrics.record_http("/v2/volumes/{volume_id}", 500, Duration::from_secs(60));

        let rendered = metrics.render();
        assert!(rendered.contains(
            "http_request_duration_seconds_bucket{class=\"2xx\",\
             route=\"/v2/volumes/{volume_id}\",le=\"0.005\"} 1"
        ));
        assert!(rendered.contains(
            "http_request_duration_seconds_bucket{class=\"2xx\",\
             route=\"/v2/volumes/{volume_id}\",le=\"0.1\"} 1"
        ));
        assert!(rendered.contains(
            "http_request_duration_seconds_bucket{class=\"2xx\",\
             route=\"/v2/volumes/{volume_id}\",le=\"0.25\"} 2"
        ));
        assert!(rendered.contains(
            "http_request_duration_seconds_bucket{class=\"2xx\",\
             route=\"/v2/volumes/{volume_id}\",le=\"+Inf\"} 2"
        ));
        assert!(rendered.contains(
            "http_request_duration_seconds_count{class=\"2xx\",\
             route=\"/v2/volumes/{volume_id}\"} 2"
        ));
        // The 60s observation overflows into the +Inf bucket only.
        assert!(rendered.contains(
            "http_request_duration_seconds_bucket{class=\"5xx\",\
             route=\"/v2/volumes/{volume_id}\",le=\"5\"} 0"
        ));
        assert!(rendered.contains(
            "http_request_duration_seconds_bucket{class=\"5xx\",\
             route=\"/v2/volumes/{volume_id}\",le=\"+Inf\"} 1"
        ));
        // The exact status code stays on the counter; latency uses classes.
        assert!(
            rendered
                .contains("http_requests_total{code=\"200\",route=\"/v2/volumes/{volume_id}\"} 2")
        );
        assert!(!rendered.contains("class=\"200\""));
    }

    #[test]
    fn label_values_are_escaped_and_classes_are_fixed() {
        assert_eq!(escape_label_value("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(status_class(199), "1xx");
        assert_eq!(status_class(204), "2xx");
        assert_eq!(status_class(302), "3xx");
        assert_eq!(status_class(429), "4xx");
        assert_eq!(status_class(503), "5xx");
    }
}
