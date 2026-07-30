//! SRT (Secure Reliable Transport) ingest for zlmediakit-rs.
//!
//! Provides an SRT listener that accepts incoming SRT connections
//! and publishes the received media stream to the shared
//! `MediaSourceManager`.

pub mod ffi;
pub mod server;

pub use server::{SrtServer, SrtServerConfig};
