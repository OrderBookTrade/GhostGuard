//! Phase 3: auto-defense (auto-cancel + blacklist).
//!
//! TODO(phase-3):
//! - On ghost detection, cancel all open orders on the affected market via CLOB REST API.
//! - Maintain a persistent blacklist file (`data/blacklist.txt`) of counterparties.
//! - Subscribe to fill stream and emit `BlacklistWarning` when a listed taker appears.
//!
//! Config hooks already parsed (see `config.rs::DefenseSection`):
//!   auto_cancel, auto_blacklist
//!
//! Intentionally empty until Phase 3 implementation lands.
