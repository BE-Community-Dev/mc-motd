//! Pure protocol encoding and decoding.
//!
//! This module has no network logic, no caching, and no query orchestration:
//! it converts between bytes and typed values so custom transports, proxies,
//! and tests can reuse the codec work. Network queries live in the edition
//! modules ([`crate::java`], [`crate::bedrock`]) and [`crate::client`].

/// Java Edition status protocol codec.
#[cfg(feature = "java")]
pub mod java;

/// Bedrock Edition protocol codecs.
#[cfg(feature = "bedrock")]
pub mod bedrock;
