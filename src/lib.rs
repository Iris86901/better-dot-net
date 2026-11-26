//! Shared domain modules, independent of the browser gateway and transport.
#[cfg(feature = "advanced")]
pub mod collaboration;
#[cfg(any(feature = "advanced", feature = "collab-cli"))]
pub mod mux;
