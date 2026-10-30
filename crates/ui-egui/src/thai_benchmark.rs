//! Benchmark-only adapter diagnostics used by the real-grid performance example.

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PassCounters {
    pub requests: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub segmentations: usize,
    pub measurements: usize,
    pub measured_bytes: usize,
    pub transient_fallbacks: usize,
    pub permanent_fallbacks: usize,
    pub exhausted: bool,
    pub repaint_retry: bool,
    pub retry_passes: usize,
    pub no_progress_passes: usize,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AdapterCounters {
    pub pass: Option<PassCounters>,
    pub context_cache_entries: usize,
    pub context_cache_accounted_bytes: usize,
    pub global_cache_entries: usize,
    pub global_cache_accounted_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    /// `None` means this revision has no Thai adapter or diagnostics are not compiled in.
    pub adapter: Option<AdapterCounters>,
}

#[cfg(feature = "thai-benchmark-diagnostics")]
pub fn snapshot(ctx: &egui::Context) -> Snapshot {
    let counters = crate::thai_layout::benchmark_diagnostics(ctx);
    Snapshot {
        adapter: Some(AdapterCounters {
            pass: Some(PassCounters {
                requests: counters.pass.requests,
                cache_hits: counters.pass.cache_hits,
                cache_misses: counters.pass.cache_misses,
                segmentations: counters.pass.segmentations,
                measurements: counters.pass.measurements,
                measured_bytes: counters.pass.measured_bytes,
                transient_fallbacks: counters.pass.transient_fallbacks,
                permanent_fallbacks: counters.pass.permanent_fallbacks,
                exhausted: counters.pass.exhausted,
                repaint_retry: counters.pass.repaint_retry,
                retry_passes: counters.pass.retry_passes,
                no_progress_passes: counters.pass.no_progress_passes,
            }),
            context_cache_entries: counters.context_cache_entries,
            context_cache_accounted_bytes: counters.context_cache_accounted_bytes,
            global_cache_entries: counters.global_cache_entries,
            global_cache_accounted_bytes: counters.global_cache_accounted_bytes,
        }),
    }
}

#[cfg(not(feature = "thai-benchmark-diagnostics"))]
pub fn snapshot(_ctx: &egui::Context) -> Snapshot {
    Snapshot { adapter: None }
}

#[cfg(feature = "thai-benchmark-diagnostics")]
pub fn clear_context(ctx: &egui::Context) {
    crate::thai_layout::clear_context_cache_for_benchmark(ctx);
}

#[cfg(not(feature = "thai-benchmark-diagnostics"))]
pub fn clear_context(_ctx: &egui::Context) {}
