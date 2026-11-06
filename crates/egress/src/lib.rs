//! Outbound safety: which destinations AgentLater may call, which headers a caller may set,
//! and how each delivery is signed.
//!
//! This crate checks URL *syntax* only. Workers cannot resolve DNS before `fetch`, so the
//! address-level guarantee (no private, loopback or reserved IPs, including via DNS) comes
//! from the API Worker's `global_fetch_strictly_public` compatibility flag. Both checks run
//! on every attempt, not just when a task is accepted.

mod destination;
mod headers;
mod signing;

pub use destination::{Destination, DestinationError, MAX_URL_BYTES};
pub use headers::{validate_header, HeaderError};
pub use signing::{
    sign, verify, Signature, SignatureError, HEADER_ATTEMPT, HEADER_SIGNATURE, HEADER_TASK_ID,
    HEADER_TIMESTAMP, SIGNATURE_TOLERANCE_SECS,
};
