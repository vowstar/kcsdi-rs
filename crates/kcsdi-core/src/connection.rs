// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Huang Rui <vowstar@gmail.com>

//! Explicit connection targets sharing one protocol session implementation.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};

use crate::control::CancellationToken;
use crate::model::Model;
use crate::transport::serial::SerialTransport;
use crate::transport::{TcpTransport, Transport};
use crate::{Device, Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ConnectionTarget {
    Tcp { host: String, port: u16 },
    Serial { path: String },
}

impl Default for ConnectionTarget {
    fn default() -> Self {
        Self::Tcp {
            host: String::new(),
            port: 901,
        }
    }
}

// Missing kind preserves previously saved TCP targets. An unknown explicit
// kind is an error, never an implicit change to a different transport.
impl<'de> Deserialize<'de> for ConnectionTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum Kind {
            #[default]
            Tcp,
            Serial,
        }

        #[derive(Deserialize)]
        #[serde(default)]
        struct Fields {
            kind: Kind,
            host: String,
            port: u16,
            path: String,
        }

        impl Default for Fields {
            fn default() -> Self {
                Self {
                    kind: Kind::Tcp,
                    host: String::new(),
                    port: 901,
                    path: String::new(),
                }
            }
        }

        let fields = Fields::deserialize(deserializer)?;
        Ok(match fields.kind {
            Kind::Tcp => Self::Tcp {
                host: fields.host,
                port: fields.port,
            },
            Kind::Serial => Self::Serial { path: fields.path },
        })
    }
}

impl ConnectionTarget {
    /// Validate without resolving a hostname or opening a port.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Tcp { host, port } if valid_host(host) && *port != 0 => Ok(()),
            Self::Tcp { .. } => Err(Error::InvalidParameter(
                "TCP requires an IP address or hostname and a nonzero port".into(),
            )),
            Self::Serial { path } => crate::transport::serial::validate_path(path),
        }
    }

    pub fn connect_controlled(
        &self,
        model: Model,
        cancel: &CancellationToken,
    ) -> Result<Device<ConnectionTransport>> {
        cancel.check()?;
        self.validate()?;
        let transport = match self {
            Self::Tcp { host, port } => {
                ConnectionTransport::Tcp(TcpTransport::connect_controlled(host, *port, cancel)?)
            }
            Self::Serial { path } => ConnectionTransport::Serial(SerialTransport::open_controlled(
                path,
                model.capabilities().serial_baud,
                cancel,
            )?),
        };
        cancel.check()?;
        let mut device = Device::with_model(transport, model);
        device.handshake_controlled(cancel)?;
        Ok(device)
    }
}

fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

impl fmt::Display for ConnectionTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp { host, port } if host.contains(':') => {
                write!(formatter, "[{host}]:{port}")
            }
            Self::Tcp { host, port } => write!(formatter, "{host}:{port}"),
            Self::Serial { path } => formatter.write_str(path),
        }
    }
}

pub enum ConnectionTransport {
    Tcp(TcpTransport),
    Serial(SerialTransport),
}

impl From<TcpTransport> for ConnectionTransport {
    fn from(transport: TcpTransport) -> Self {
        Self::Tcp(transport)
    }
}

impl Transport for ConnectionTransport {
    fn send_with_timeout(&mut self, data: &[u8], timeout: Duration) -> Result<()> {
        match self {
            Self::Tcp(transport) => transport.send_with_timeout(data, timeout),
            Self::Serial(transport) => transport.send_with_timeout(data, timeout),
        }
    }

    fn recv_line(&mut self, timeout: Duration) -> Result<String> {
        match self {
            Self::Tcp(transport) => transport.recv_line(timeout),
            Self::Serial(transport) => transport.recv_line(timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_does_not_require_serial_enumeration_or_name_resolution() {
        for target in [
            ConnectionTarget::Tcp {
                host: "instrument.example.invalid".into(),
                port: 901,
            },
            ConnectionTarget::Tcp {
                host: "::1".into(),
                port: 1,
            },
            ConnectionTarget::Serial {
                path: "/dev/serial/by-id/unavailable".into(),
            },
            ConnectionTarget::Serial {
                path: "COM7".into(),
            },
        ] {
            assert!(target.validate().is_ok(), "{target:?}");
        }
    }

    #[test]
    fn invalid_targets_fail_before_io() {
        for host in ["", "bad host", "bad:901", "-host", "host..local", "host\n"] {
            assert!(
                ConnectionTarget::Tcp {
                    host: host.into(),
                    port: 901
                }
                .validate()
                .is_err()
            );
        }
        for path in ["", "  ", "COM7\n", "COM7\0", "COM7\t"] {
            let target = ConnectionTarget::Serial { path: path.into() };
            assert!(matches!(
                target.connect_controlled(Model::Kc901V, &CancellationToken::default()),
                Err(Error::InvalidParameter(_))
            ));
        }
        assert!(
            ConnectionTarget::Tcp {
                host: "127.0.0.1".into(),
                port: 0
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn cancelled_connect_never_opens_the_target() {
        let cancel = CancellationToken::default();
        cancel.cancel();
        for target in [
            ConnectionTarget::Tcp {
                host: "unreachable.example.invalid".into(),
                port: 901,
            },
            ConnectionTarget::Serial {
                path: "not-a-port".into(),
            },
        ] {
            assert!(matches!(
                target.connect_controlled(Model::Kc901V, &cancel),
                Err(Error::Cancelled)
            ));
        }
    }

    #[test]
    fn display_keeps_ipv6_unambiguous_and_serial_paths_intact() {
        assert_eq!(
            ConnectionTarget::Tcp {
                host: "::1".into(),
                port: 901
            }
            .to_string(),
            "[::1]:901"
        );
        assert_eq!(
            ConnectionTarget::Serial {
                path: "/dev/serial/by-id/test".into()
            }
            .to_string(),
            "/dev/serial/by-id/test"
        );
    }
}
