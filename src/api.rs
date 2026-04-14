//! Phase 4: HTTP status endpoint.
//!
//! TODO(phase-4):
//! - Listen on `config.api.listen` (default 127.0.0.1:8080) with an axum/hyper server.
//! - Routes:
//!     GET /status    -> uptime, connected markets, total fills verified
//!     GET /verdicts  -> last 100 verdicts (tail JSONL log)
//!     GET /blacklist -> current blacklisted addresses (reads defense state)
//!     GET /stats     -> ghost rate %, avg verification latency, predictive accuracy
//!
//! Config hook already parsed (see `config.rs::ApiSection`):
//!   listen
//!
//! Intentionally empty until Phase 4 implementation lands.
