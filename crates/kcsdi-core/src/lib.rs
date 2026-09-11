// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! KC901 instrument protocol library.
//!
//! Blocking I/O implementation of the KC901 text protocol documented in
//! the project protocol reference:
//!
//! - [`transport`]: line-oriented TCP and serial transports,
//! - [`protocol`]: `$start`/`$end` packet and measurement stream parsers,
//! - [`commands`]: byte-exact command builders,
//! - [`device`]: high-level session API (handshake, info queries, sweeps),
//! - [`model`]: per-model capability tables,
//! - [`data`]: identity and measurement data types,
//! - [`error`]: shared error type.

pub mod atomic_file;
pub mod calibration;
pub mod commands;
pub mod connection;
pub mod control;
pub mod data;
pub mod device;
pub mod discovery;
pub mod error;
pub mod model;
pub mod protocol;
pub mod segments;
pub mod source;
pub mod table;
pub mod touchstone;
pub mod transport;
pub mod validation;

pub use device::Device;
pub use error::{Error, Result};
