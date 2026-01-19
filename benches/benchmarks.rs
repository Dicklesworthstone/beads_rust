// Canonical benchmark entrypoint that reuses the storage_perf benchmark suite.
// This keeps the benchmark set in one place while exposing the expected name.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::significant_drop_tightening
)]
include!("storage_perf.rs");
