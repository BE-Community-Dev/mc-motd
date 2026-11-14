//! Bedrock Edition protocol codecs.

/// Legacy `RakNet` offline-ping codec.
pub mod raknet;

/// `NetherNet` join endpoint codec.
#[cfg(feature = "nethernet")]
pub mod nethernet;
