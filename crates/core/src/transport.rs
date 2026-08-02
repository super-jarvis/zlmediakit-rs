use arc_swap::ArcSwap;
use serde::Serialize;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::rustls;
pub use tokio_rustls::TlsAcceptor;

static TLS_ACCEPTORS: LazyLock<Mutex<Vec<Weak<ReloadableTlsAcceptor>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// TLS server configuration shared by a listener. Each accepted connection
/// snapshots the current rustls config, so a successful reload affects new
/// handshakes without interrupting established sessions.
pub struct ReloadableTlsAcceptor {
    config: ArcSwap<rustls::ServerConfig>,
    cert_path: String,
    key_path: String,
}

impl ReloadableTlsAcceptor {
    fn new(cert_path: String, key_path: String) -> anyhow::Result<Arc<Self>> {
        let config = read_tls_server_config(&cert_path, &key_path)?;
        let acceptor = Arc::new(Self {
            config: ArcSwap::from(config),
            cert_path,
            key_path,
        });
        TLS_ACCEPTORS
            .lock()
            .unwrap()
            .push(Arc::downgrade(&acceptor));
        Ok(acceptor)
    }

    /// Reloads the configured PEM files. The previous working config remains
    /// active if reading, parsing, or certificate/key validation fails.
    pub fn reload(&self) -> anyhow::Result<()> {
        let config = read_tls_server_config(&self.cert_path, &self.key_path)?;
        self.config.store(config);
        Ok(())
    }

    pub async fn accept(
        &self,
        stream: TcpStream,
    ) -> Result<tokio_rustls::server::TlsStream<TcpStream>, io::Error> {
        TlsAcceptor::from(self.config.load_full())
            .accept(stream)
            .await
    }

    pub fn description(&self) -> String {
        format!("{} / {}", self.cert_path, self.key_path)
    }
}

#[derive(Debug, Default, Serialize)]
pub struct TlsReloadReport {
    pub reloaded: usize,
    pub failed: Vec<String>,
}

/// Reloads every currently-live TLS listener registered in this process.
/// Dead listener entries are pruned while the snapshot is collected.
pub fn reload_registered_tls() -> TlsReloadReport {
    let acceptors = {
        let mut registered = TLS_ACCEPTORS.lock().unwrap();
        let mut live = Vec::new();
        registered.retain(|entry| {
            if let Some(acceptor) = entry.upgrade() {
                live.push(acceptor);
                true
            } else {
                false
            }
        });
        live
    };
    let mut report = TlsReloadReport::default();
    for acceptor in acceptors {
        match acceptor.reload() {
            Ok(()) => report.reloaded += 1,
            Err(error) => report
                .failed
                .push(format!("{}: {error}", acceptor.description())),
        }
    }
    report
}

/// A transport stream that can be either a plain TCP connection or a TLS-wrapped
/// connection.  Session types that previously accepted `TcpStream` directly can
/// accept `TransportStream` instead, allowing the server to add TLS support
/// without changing any session logic.
pub enum TransportStream {
    Tcp(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl TransportStream {
    /// Wrap the result of `TlsAcceptor::accept` into a `TransportStream`.
    pub fn from_tls_accepted(accepted: tokio_rustls::server::TlsStream<TcpStream>) -> Self {
        TransportStream::Tls(Box::new(accepted))
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
) -> anyhow::Result<Option<Arc<ReloadableTlsAcceptor>>> {
    let (cert, key) = match (cert_path, key_path) {
        (Some(c), Some(k)) => (c, k),
        _ => return Ok(None),
    };
    Ok(Some(ReloadableTlsAcceptor::new(
        cert.to_string(),
        key.to_string(),
    )?))
}

fn read_tls_server_config(cert: &str, key: &str) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let certs_bytes = std::fs::read(cert)?;
    let key_bytes = std::fs::read(key)?;

    let certs: Vec<rustls::pki_types::CertificateDer> =
        rustls_pemfile::certs(&mut &*certs_bytes).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        anyhow::bail!("no certificate found in {}", cert);
    }

    let key = rustls_pemfile::private_key(&mut &*key_bytes)?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", key))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, KeyPair};

    fn certificate() -> (rustls::pki_types::CertificateDer<'static>, String, String) {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["127.0.0.1".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.der().clone(), cert.pem(), key.serialize_pem())
    }

    fn client_config(
        certificates: &[rustls::pki_types::CertificateDer<'static>],
    ) -> Arc<rustls::ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        for certificate in certificates {
            roots.add(certificate.clone()).unwrap();
        }
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }

    async fn peer_certificate(
        addr: std::net::SocketAddr,
        config: Arc<rustls::ClientConfig>,
    ) -> Vec<u8> {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = rustls::pki_types::ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let tls = tokio_rustls::TlsConnector::from(config)
            .connect(server_name, tcp)
            .await
            .unwrap();
        tls.get_ref().1.peer_certificates().unwrap()[0]
            .as_ref()
            .to_vec()
    }

    #[tokio::test]
    async fn tls_reload_changes_new_handshakes_and_keeps_last_good_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp = tempfile::tempdir().unwrap();
        let cert_path = temp.path().join("cert.pem");
        let key_path = temp.path().join("key.pem");
        let (der_one, pem_one, key_one) = certificate();
        let (der_two, pem_two, key_two) = certificate();
        std::fs::write(&cert_path, pem_one).unwrap();
        std::fs::write(&key_path, key_one).unwrap();

        let acceptor = load_tls_config(
            Some(cert_path.to_str().unwrap()),
            Some(key_path.to_str().unwrap()),
        )
        .unwrap()
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_acceptor = acceptor.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (stream, _) = listener.accept().await.unwrap();
                server_acceptor.accept(stream).await.unwrap();
            }
        });
        let client = client_config(&[der_one.clone(), der_two.clone()]);

        assert_eq!(
            peer_certificate(addr, client.clone()).await,
            der_one.as_ref()
        );

        std::fs::write(&cert_path, pem_two).unwrap();
        std::fs::write(&key_path, key_two).unwrap();
        let report = reload_registered_tls();
        assert!(report.failed.is_empty());
        assert!(report.reloaded >= 1);
        assert_eq!(
            peer_certificate(addr, client.clone()).await,
            der_two.as_ref()
        );

        std::fs::write(&key_path, "not a private key").unwrap();
        assert!(acceptor.reload().is_err());
        assert_eq!(peer_certificate(addr, client).await, der_two.as_ref());
        server.await.unwrap();
    }
}
