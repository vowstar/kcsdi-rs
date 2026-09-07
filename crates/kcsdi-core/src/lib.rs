// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! KC901 instrument protocol library.
//!
//! Blocking I/O implementation of the KC901 text protocol documented in
//! `the protocol reference`:
//!
//! - [`transport`]: line-oriented transports (TCP),
//! - [`protocol`]: `$start`/`$end` packet and measurement stream parsers,
//! - [`commands`]: byte-exact command builders,
//! - [`device`]: high-level session API (handshake, info queries, sweeps),
//! - [`model`]: per-model capability tables,
//! - [`data`]: identity and measurement data types,
//! - [`error`]: shared error type.

pub mod commands;
pub mod data;
pub mod device;
pub mod error;
pub mod model;
pub mod protocol;
pub mod transport;

pub use device::Device;
pub use error::{Error, Result};
