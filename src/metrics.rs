//! Process-wide counters, served at `GET /metrics` in the Prometheus text format.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

pub struct Counter {
    name: &'static str,
    help: &'static str,
    value: AtomicU64,
}

impl Counter {
    const fn new(name: &'static str, help: &'static str) -> Counter {
        Counter {
            name,
            help,
            value: AtomicU64::new(0),
        }
    }

    pub fn add(&self, n: u64) {
        self.value.fetch_add(n, Relaxed);
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Relaxed)
    }
}

/// A duration total and count, enough for averages and rates.
pub struct Timer {
    sum_micros: Counter,
    count: Counter,
}

impl Timer {
    const fn new(sum: &'static str, count: &'static str, help: &'static str) -> Timer {
        Timer {
            sum_micros: Counter::new(sum, help),
            count: Counter::new(count, help),
        }
    }

    pub fn observe(&self, d: Duration) {
        self.sum_micros.add(d.as_micros() as u64);
        self.count.inc();
    }
}

pub static FETCHES: Counter = Counter::new("pando_fetches_total", "Fetches served.");
pub static FETCH_ERRORS: Counter = Counter::new("pando_fetch_errors_total", "Fetches that failed.");
pub static PUSHES: Counter = Counter::new("pando_pushes_total", "Pushes received.");
pub static REFS_UPDATED: Counter =
    Counter::new("pando_refs_updated_total", "Ref updates accepted.");
pub static REFS_REJECTED: Counter =
    Counter::new("pando_refs_rejected_total", "Ref updates rejected.");
pub static PUSH_BYTES: Counter = Counter::new("pando_push_bytes_total", "Pack bytes received.");
pub static PACK_BYTES_SERVED: Counter = Counter::new(
    "pando_pack_bytes_served_total",
    "Stored pack bytes served by URL (this node's Express tier).",
);
pub static SSH_SESSIONS: Counter = Counter::new("pando_ssh_sessions_total", "SSH commands run.");
pub static ACCESS_DENIED: Counter = Counter::new(
    "pando_auth_denied_total",
    "Authenticated requests refused for lack of access.",
);
pub static GC_PACKS_DELETED: Counter =
    Counter::new("pando_gc_packs_deleted_total", "Packs deleted by gc.");
pub static GC_BYTES_DELETED: Counter =
    Counter::new("pando_gc_bytes_deleted_total", "Bytes freed by gc.");
pub static WEBHOOK_FAILURES: Counter = Counter::new(
    "pando_webhook_failures_total",
    "Webhook deliveries that failed.",
);
pub static PUSH_TIME: Timer = Timer::new(
    "pando_push_seconds_sum",
    "pando_push_seconds_count",
    "Time spent handling pushes.",
);
pub static FETCH_TIME: Timer = Timer::new(
    "pando_fetch_seconds_sum",
    "pando_fetch_seconds_count",
    "Time spent handling fetch requests.",
);

/// Render every metric.
pub fn render() -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# TYPE pando_build_info gauge\npando_build_info{{version=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION")
    );
    for c in [
        &FETCHES,
        &FETCH_ERRORS,
        &PUSHES,
        &REFS_UPDATED,
        &REFS_REJECTED,
        &PUSH_BYTES,
        &PACK_BYTES_SERVED,
        &SSH_SESSIONS,
        &ACCESS_DENIED,
        &GC_PACKS_DELETED,
        &GC_BYTES_DELETED,
        &WEBHOOK_FAILURES,
    ] {
        let _ = writeln!(
            out,
            "# HELP {0} {1}\n# TYPE {0} counter\n{0} {2}",
            c.name,
            c.help,
            c.get()
        );
    }
    for t in [&PUSH_TIME, &FETCH_TIME] {
        let _ = writeln!(
            out,
            "# HELP {0} {1}\n# TYPE {0} counter\n{0} {2}",
            t.sum_micros.name,
            t.sum_micros.help,
            t.sum_micros.get() as f64 / 1e6
        );
        let _ = writeln!(
            out,
            "# TYPE {0} counter\n{0} {1}",
            t.count.name,
            t.count.get()
        );
    }
    out
}
