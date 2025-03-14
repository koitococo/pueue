//! Socket handling is platform specific code.
//!
//! The submodules of this module represent the different implementations for
//! each supported platform.
//! Depending on the target, the respective platform is read and loaded into this scope.

use std::io::{Read, Write};
use std::net::TcpStream;
#[cfg(not(target_os = "windows"))]
use std::path::PathBuf;

use rustls::pki_types::CertificateDer;
use rustls_connector::{RustlsConnector as TlsConnector, RustlsConnectorConfig};

use crate::error::Error;
#[cfg(feature = "settings")]
use crate::{settings::Shared, tls::load_ca};

/// Shared socket logic
#[cfg_attr(not(target_os = "windows"), path = "unix.rs")]
#[cfg_attr(target_os = "windows", path = "windows.rs")]
mod platform;
pub use platform::*;

/// A new trait, which can be used to represent Unix- and TcpListeners. \
/// This is necessary to easily write generic functions where both types can be used.
pub trait BlockingListener: Sync + Send {
    fn accept(&self) -> Result<GenericBlockingStream, Error>;
}

/// Convenience type, so we don't have type write `Box<dyn Listener>` all the time.
pub type GenericBlockingListener = Box<dyn BlockingListener>;
/// Convenience type, so we don't have type write `Box<dyn Stream>` all the time. \
/// This also prevents name collisions, since `Stream` is imported in many preludes.
pub type GenericBlockingStream = Box<dyn BlockingStream>;

/// Describe how a client should connect to the daemon.
pub enum ConnectionSettings<'a> {
    #[cfg(not(target_os = "windows"))]
    UnixSocket { path: PathBuf },
    TlsTcpSocket {
        host: String,
        port: String,
        certificate: CertificateDer<'a>,
    },
}

/// Convenience conversion from [Shared] to [ConnectionSettings].
#[cfg(feature = "settings")]
impl TryFrom<Shared> for ConnectionSettings<'_> {
    type Error = crate::error::Error;

    fn try_from(value: Shared) -> Result<Self, Self::Error> {
        // Unix socket handling
        #[cfg(not(target_os = "windows"))]
        {
            if value.use_unix_socket {
                return Ok(ConnectionSettings::UnixSocket {
                    path: value.unix_socket_path(),
                });
            }
        }

        let cert = load_ca(&value.daemon_cert())?;
        Ok(ConnectionSettings::TlsTcpSocket {
            host: value.host,
            port: value.port,
            certificate: cert,
        })
    }
}

pub trait BlockingStream: Read + Write {}
impl BlockingStream for rustls_connector::TlsStream<TcpStream> {}

/// Initialize our client [TlsConnector]. \
/// 1. Trust our own CA. ONLY our own CA.
/// 2. Set the client certificate and key
pub fn get_tls_connector(cert: CertificateDer<'_>) -> Result<TlsConnector, Error> {
    let mut config = RustlsConnectorConfig::default();
    config.add_parsable_certificates(vec![cert]);
    let connector = config.connector_with_no_client_auth();

    Ok(connector)
}
