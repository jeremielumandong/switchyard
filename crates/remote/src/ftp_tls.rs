//! TLS for FTP connections: rustls (tokio-rustls) behind suppaftp's connector traits, with a
//! graceful close.
//!
//! suppaftp's own rustls stream closes a data connection with `close_notify` and drops the
//! socket at once. A TLS 1.3 server sends session tickets after the handshake, which an upload
//! never reads; closing a socket with unread data makes the kernel send RST instead of FIN and
//! discard what it has not sent yet. On a slow link (or through a proxy) the server then loses
//! the tail of the upload and answers `426 Failure reading network stream`. [`FtpTlsStream`]
//! shuts its write side down and then reads until the server closes too (bounded by
//! [`DRAIN_TIMEOUT`]), so the socket is empty when it is dropped.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use rustls::pki_types::ServerName;
use suppaftp::tokio::{AsyncTlsConnector, TokioTlsStream};
use suppaftp::{FtpError, FtpResult};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

/// How long a closing data connection waits for the server to close its side.
pub(crate) const DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Makes [`FtpTlsStream`]s for the control and data connections.
#[derive(Clone)]
pub(crate) struct FtpTlsConnector(tokio_rustls::TlsConnector);

impl FtpTlsConnector {
    pub(crate) fn new(config: Arc<rustls::ClientConfig>) -> Self {
        Self(tokio_rustls::TlsConnector::from(config))
    }
}

impl std::fmt::Debug for FtpTlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FtpTlsConnector")
    }
}

// The trait is declared with `#[async_trait]`; this is its expanded signature.
impl AsyncTlsConnector for FtpTlsConnector {
    type Stream = FtpTlsStream;

    fn connect<'life0, 'life1, 'async_trait>(
        &'life0 self,
        domain: &'life1 str,
        stream: TcpStream,
    ) -> Pin<Box<dyn Future<Output = FtpResult<Self::Stream>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let name = ServerName::try_from(domain.to_owned())
                .map_err(|e| FtpError::SecureError(e.to_string()))?;
            let inner = self
                .0
                .connect(name, stream)
                .await
                .map_err(|e| FtpError::SecureError(e.to_string()))?;
            Ok(FtpTlsStream {
                inner,
                close: Close::Open,
            })
        })
    }
}

enum Close {
    Open,
    /// `close_notify` and FIN sent; reading until the server closes.
    Draining(Pin<Box<tokio::time::Sleep>>),
    Closed,
}

/// A TLS connection that, on shutdown, waits for the server to close before it is dropped.
pub struct FtpTlsStream {
    inner: TlsStream<TcpStream>,
    close: Close,
}

impl std::fmt::Debug for FtpTlsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FtpTlsStream")
    }
}

impl AsyncRead for FtpTlsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FtpTlsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            match &mut this.close {
                Close::Open => {
                    // close_notify, flush, then FIN.
                    ready!(Pin::new(&mut this.inner).poll_shutdown(cx))?;
                    this.close = Close::Draining(Box::pin(tokio::time::sleep(DRAIN_TIMEOUT)));
                }
                Close::Draining(deadline) => {
                    let mut scratch = [0u8; 4096];
                    let mut buf = ReadBuf::new(&mut scratch);
                    match Pin::new(&mut this.inner).poll_read(cx, &mut buf) {
                        // Tickets or other late records: keep reading.
                        Poll::Ready(Ok(())) if !buf.filled().is_empty() => continue,
                        // The server closed (close_notify or EOF); a reset or a missing
                        // close_notify is the server's to report on the control connection.
                        Poll::Ready(_) => this.close = Close::Closed,
                        Poll::Pending => {
                            ready!(deadline.as_mut().poll(cx));
                            this.close = Close::Closed;
                        }
                    }
                }
                // A second shutdown (suppaftp's after ours) must not touch the closed socket.
                Close::Closed => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl TokioTlsStream for FtpTlsStream {
    type InnerStream = TlsStream<TcpStream>;

    fn tcp_stream(self) -> FtpResult<TcpStream> {
        Ok(self.inner.into_inner().0)
    }

    fn get_ref(&self) -> &TcpStream {
        self.inner.get_ref().0
    }

    fn mut_ref(&mut self) -> &mut Self::InnerStream {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const CA: &str = include_str!("../tests/fixtures/ftp-tls/ca.pem");
    const CERT: &str = include_str!("../tests/fixtures/ftp-tls/cert.pem");
    const KEY: &str = include_str!("../tests/fixtures/ftp-tls/key.pem");

    fn provider() -> Arc<rustls::crypto::CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    /// The CI failure: a TLS 1.3 server's session tickets sit unread in the client socket,
    /// so dropping it right after `close_notify` sends RST and the server, still reading the
    /// upload, loses its tail (vsftpd: `426 Failure reading network stream`). The stream
    /// must stay open until the server has read everything.
    #[tokio::test]
    async fn upload_reaches_a_slow_reader_whole() {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(CERT.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        let key = PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let reader = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            // Busy elsewhere while the client finishes: the upload waits in buffers.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let mut got = Vec::new();
            let r = tls.read_to_end(&mut got).await.map(|_| got.len());
            let _ = tls.shutdown().await;
            r
        });

        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(CA.as_bytes()) {
            roots.add(c.unwrap()).unwrap();
        }
        let client = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = TcpStream::connect(addr).await.unwrap();
        let mut tls = FtpTlsConnector::new(Arc::new(client))
            .connect("localhost", tcp)
            .await
            .unwrap();
        let data = vec![7u8; 8 * 1024 * 1024];
        tls.write_all(&data).await.unwrap();
        tls.shutdown().await.unwrap();
        // suppaftp shuts the stream down a second time when it finishes the transfer.
        tls.shutdown().await.unwrap();
        drop(tls);
        assert_eq!(reader.await.unwrap().unwrap(), data.len());
    }
}
