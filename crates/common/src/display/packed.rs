use crate::geom::Rect;

/// Retained with the candidate so an idle refusal survives the next trace reset.
#[derive(Clone, Copy, Debug)]
pub struct PackDiagnostic {
    pub reason: &'static str,
    pub needed_bytes: usize,
    pub available_bytes: usize,
}
impl PackDiagnostic {
    pub const fn new(reason: &'static str) -> Self {
        Self { reason, needed_bytes: 0, available_bytes: 0 }
    }
    pub fn record(self) {
        if crate::perf_trace::recording() {
            crate::perf_trace::packed_span_reason(self.reason);
            crate::perf_trace::packed_span_budget(self.needed_bytes, self.available_bytes);
        }
    }
}
impl Default for PackDiagnostic {
    fn default() -> Self { Self::new("no_pack_built") }
}

#[cfg_attr(not(any(test, feature = "miyoo", feature = "perf-bench")), allow(dead_code))]
pub struct PackedSpans {
    pub(crate) layout: [u64; 32],
    pub(crate) base: usize,
    pub(crate) candidate: usize,
    pub(crate) revision: u64,
    pub(crate) damage: Vec<Rect>,
    pub(crate) rows: Vec<PackedRow>,
    pub(crate) bytes: Vec<u8>,
}

#[cfg_attr(not(any(test, feature = "miyoo", feature = "perf-bench")), allow(dead_code))]
pub(crate) struct PackedRow {
    pub y: usize,
    pub x0: usize,
    pub x1: usize,
    pub start: usize,
}

impl PackedSpans {
    pub fn charge(&self) -> usize {
        std::mem::size_of::<Self>() + self.damage.capacity() * std::mem::size_of::<Rect>()
            + self.rows.capacity() * std::mem::size_of::<PackedRow>() + self.bytes.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepack_budget_diagnostic_serializes_and_replays_after_render_reset() {
        let path = std::env::temp_dir().join(format!("prepack-budget-trace-{}.jsonl", std::process::id()));
        crate::perf_trace::start_probe(path.to_str().unwrap());
        let diagnostic = PackDiagnostic { reason: "pack_budget", needed_bytes: 462_736, available_bytes: 120_000 };
        diagnostic.record();
        crate::perf_trace::begin_render();
        assert_eq!(crate::perf_trace::probe_counts()["packed_span_needed_bytes"], 0);
        assert_eq!(crate::perf_trace::probe_counts()["packed_span_available_bytes"], 0);
        diagnostic.record();
        crate::perf_trace::drawn_frame();
        PackDiagnostic::new("pack_size").record();
        assert_eq!(crate::perf_trace::probe_counts()["packed_span_needed_bytes"], 0);
        assert_eq!(crate::perf_trace::probe_counts()["packed_span_available_bytes"], 0);
        crate::perf_trace::stop_probe();
        let lines = std::fs::read_to_string(&path).unwrap();
        let frame: serde_json::Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
        assert_eq!(frame["event"], "frame");
        assert_eq!(frame["packed_span_reason"], "pack_budget");
        assert_eq!(frame["packed_span_needed_bytes"], diagnostic.needed_bytes);
        assert_eq!(frame["packed_span_available_bytes"], diagnostic.available_bytes);
        std::fs::remove_file(path).unwrap();
    }
}
