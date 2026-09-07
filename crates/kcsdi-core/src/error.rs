// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Error type shared by all kcsdi-core modules.

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// All failure modes of the protocol library.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Underlying I/O failure (socket closed, write failed, ...).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A read did not complete within its deadline.
    #[error("operation timed out")]
    Timeout,

    /// The byte stream did not match the documented protocol.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The connection was closed by the peer (or never established).
    #[error("not connected")]
    NotConnected,

    /// The instrument refused remote control (e.g. a front panel window is
    /// open, packet `ConFail`).
    #[error("device busy: {0}")]
    DeviceBusy(String),

    /// The instrument answered with an `err_*` packet; the payload is the
    /// packet name (e.g. `err_par1`). Match on the name, never body text.
    #[error("device error packet: {0}")]
    Device(String),
}
