use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

pub(crate) trait ClientIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ClientIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

pub(crate) type ClientStream = Box<dyn ClientIo>;

pub(crate) async fn connect(host: &str, port: u16, secure: bool) -> Result<ClientStream> {
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, port)))
        .await
        .context("RTSP connect timed out")?
        .with_context(|| format!("failed to connect RTSP upstream {host}:{port}"))?;
    if !secure {
        return Ok(Box::new(tcp));
    }

    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    connect_tls(tcp, host, Arc::new(config)).await
}

async fn connect_tls(
    tcp: TcpStream,
    host: &str,
    config: Arc<ClientConfig>,
) -> Result<ClientStream> {
    let server_name =
        ServerName::try_from(host.to_string()).context("invalid RTSPS server name")?;
    let tls = tokio::time::timeout(
        Duration::from_secs(10),
        TlsConnector::from(config).connect(server_name, tcp),
    )
    .await
    .context("RTSPS handshake timed out")?
    .context("RTSPS certificate verification or handshake failed")?;
    Ok(Box::new(tls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::generate_simple_self_signed;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::rustls::pki_types::PrivatePkcs8KeyDer;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    #[tokio::test]
    async fn establishes_validated_tls_connection() {
        let certified = generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let certificate = certified.cert.der().clone();
        let private_key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key.into())
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = TlsAcceptor::from(Arc::new(server_config))
                .accept(tcp)
                .await
                .unwrap();
            let mut request = [0u8; 4];
            tls.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            tls.write_all(b"pong").await.unwrap();
        });

        let mut roots = RootCertStore::empty();
        roots.add(certificate).unwrap();
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let mut client = connect_tls(tcp, "localhost", Arc::new(client_config))
            .await
            .unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut response = [0u8; 4];
        client.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");
        server.await.unwrap();
    }
}
