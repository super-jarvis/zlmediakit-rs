use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls;
pub use tokio_rustls::TlsAcceptor;

/// A transport stream that can be either a plain TCP connection or a TLS-wrapped
/// connection.  Session types that previously accepted `TcpStream` directly can
/// accept `TransportStream` instead, allowing the server to add TLS support
/// without changing any session logic.
pub enum TransportStream {
    Tcp(TcpStream),
    Tls(tokio_rustls::server::TlsStream<TcpStream>),
}

impl TransportStream {
    /// Wrap the result of `TlsAcceptor::accept` into a `TransportStream`.
    pub fn from_tls_accepted(accepted: tokio_rustls::server::TlsStream<TcpStream>) -> Self {
        TransportStream::Tls(accepted)
    }

    pub fn peer_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            TransportStream::Tcp(s) => s.peer_addr(),
            TransportStream::Tls(s) => s.get_ref().0.peer_addr(),
        }
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        match self {
            TransportStream::Tcp(s) => s.local_addr(),
            TransportStream::Tls(s) => s.get_ref().0.local_addr(),
        }
    }
}

impl AsyncRead for TransportStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportStream::Tcp(ref mut s) => Pin::new(s).poll_read(cx, buf),
            TransportStream::Tls(ref mut s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for TransportStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TransportStream::Tcp(ref mut s) => Pin::new(s).poll_write(cx, buf),
            TransportStream::Tls(ref mut s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportStream::Tcp(ref mut s) => Pin::new(s).poll_flush(cx),
            TransportStream::Tls(ref mut s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TransportStream::Tcp(ref mut s) => Pin::new(s).poll_shutdown(cx),
            TransportStream::Tls(ref mut s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Attempts to build a `TlsAcceptor` from PEM-encoded certificate chain and
/// private key files.  Returns `None` when either path is missing (plain TCP
/// mode); returns an error when the files cannot be read or parsed.
pub fn load_tls_config(
    cert_path: Option<&str>,
    key_path: Option<&str>,
) -> anyhow::Result<Option<TlsAcceptor>> {
    let (cert, key) = match (cert_path, key_path) {
        (Some(c), Some(k)) => (c, k),
        _ => return Ok(None),
    };
    let certs_bytes = std::fs::read(cert)?;
    let key_bytes = std::fs::read(key)?;

    let certs: Vec<rustls::pki_types::CertificateDer> = rustls_pemfile::certs(&mut &*certs_bytes)
        .filter_map(|r| r.ok())
        .collect();
    if certs.is_empty() {
        anyhow::bail!("no certificate found in {}", cert);
    }

    let key = rustls_pemfile::private_key(&mut &*key_bytes)?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}
