//! In-process Prometheus counters, hand-formatted (no prometheus crate).
//!
//! Two counter families are exposed on `/metrics` in the Prometheus text
//! exposition format:
//!
//! - `http_requests_total{route,code}`: HTTP requests by matched route
//!   pattern and response status code;
//! - `operations_total{kind,outcome}`: volume operations by kind
//!   (`create_volume`, ...) and outcome (`success`, `failure`, `replayed`,
//!   `in_doubt`, `conflict`).
//!
//! Counters are [`AtomicU64`]s held in maps guarded by a short-lived
//! [`std::sync::Mutex`]; nothing in this module ever awaits while holding a
//! lock.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Prometheus counters for the HTTP surface of one API server instance.
#[derive(Debug, Default)]
pub struct Metrics {
    /// `http_requests_total` labels: (route pattern, status code).
    http_requests: Mutex<BTreeMap<(String, u16), AtomicU64>>,
    /// `operations_total` labels: (operation kind, outcome).
    operations: Mutex<BTreeMap<(&'static str, &'static str), AtomicU64>>,
}

impl Metrics {
    /// A fresh, empty set of counters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one served HTTP request.
    pub(crate) fn record_http(&self, route: &str, code: u16) {
        let mut counters = self.lock_http();
        counters
            .entry((route.to_owned(), code))
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count one resolved volume operation.
    pub(crate) fn record_operation(&self, kind: &'static str, outcome: &'static str) {
        let mut counters = self.lock_operations();
        counters
            .entry((kind, outcome))
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Render the counters in the Prometheus text exposition format
    /// (labels in alphabetical order, output deterministic).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
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
        out
    }

    fn lock_http(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, u16), AtomicU64>> {
        // Counters are advisory: a poisoned lock (a panic while counting)
        // must not take the HTTP surface down with it.
        self.http_requests
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
}

/// Escape a label value for the Prometheus text format.
fn escape_label_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
