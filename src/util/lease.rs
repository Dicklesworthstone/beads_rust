//! Lease utilities for claim protocol.

use chrono::{DateTime, Duration, Utc};
use rand::TryRngCore;
use rand::rngs::OsRng;
use std::fmt::Write as _;

/// Default lease TTL in seconds (30 minutes).
pub const DEFAULT_LEASE_TTL_SECS: i64 = 30 * 60;

/// Generate a random lease ID (32 hex chars).
#[must_use]
pub fn generate_lease_id() -> String {
    let mut bytes = [0_u8; 16];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut bytes)
        .expect("OS RNG unavailable");

    let mut out = String::with_capacity(32);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Compute lease expiration time from now + TTL seconds.
#[must_use]
pub fn lease_expires_at(now: DateTime<Utc>, ttl_seconds: i64) -> DateTime<Utc> {
    now + Duration::seconds(ttl_seconds)
}
