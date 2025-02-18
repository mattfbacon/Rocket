use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::bindable::BindableAddr;
use crate::listener::{Certificates, Connection, Listener};
use crate::tls::util::{load_ca_certs, load_certs, load_private_key};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::{server::TlsStream as BareTlsStream, Accept, TlsAcceptor};

/// A TLS listener over TCP.
pub struct TlsListener<L: Listener> {
    listener: L,
    acceptor: TlsAcceptor,
}

/// This implementation exists so that ROCKET_WORKERS=1 can make progress while
/// a TLS handshake is being completed. It does this by returning `Ready` from
/// `poll_accept()` as soon as we have a TCP connection and performing the
/// handshake in the `AsyncRead` and `AsyncWrite` implementations.
///
/// A straight-forward implementation of this strategy results in none of the
/// TLS information being available at the time the connection is "established",
/// that is, when `poll_accept()` returns, since the handshake has yet to occur.
/// Importantly, certificate information isn't available at the time that we
/// request it.
///
/// The underlying problem is hyper's "Accept" trait. Were we to manage
/// connections ourselves, we'd likely want to:
///
///   1. Stop blocking the worker as soon as we have a TCP connection.
///   2. Perform the handshake in the background.
///   3. Give the connection to Rocket when/if the handshake is done.
///
/// See hyperium/hyper/issues/2321 for more details.
///
/// To work around this, we "lie" when `peer_certificates()` are requested and
/// always return `Some(Certificates)`. Internally, `Certificates` is an
/// `Arc<Storage<Vec<CertificateData>>>`, effectively a shared, thread-safe,
/// `OnceCell`. The cell is initially empty and is filled as soon as the
/// handshake is complete. If the certificate data were to be requested prior to
/// this point, it would be empty. However, in Rocket, we only request
/// certificate data when we have a `Request` object, which implies we're
/// receiving payload data, which implies the TLS handshake has finished, so the
/// certificate data as seen by a Rocket application will always be "fresh".
pub struct TlsStream<C: Connection + Unpin> {
    remote: Option<BindableAddr>,
    state: TlsState<C>,
    certs: Certificates,
}

/// State of `TlsStream`.
pub enum TlsState<C: Connection> {
    /// The TLS handshake is taking place. We don't have a full connection yet.
    Handshaking(Accept<C>),
    /// TLS handshake completed successfully; we're getting payload data.
    Streaming(BareTlsStream<C>),
}

/// TLS as ~configured by `TlsConfig` in `rocket` core.
pub struct Config<R> {
    pub cert_chain: R,
    pub private_key: R,
    pub ciphersuites: Vec<rustls::SupportedCipherSuite>,
    pub prefer_server_order: bool,
    pub ca_certs: Option<R>,
    pub mandatory_mtls: bool,
}

impl<L: Listener> TlsListener<L> {
    pub async fn bind<R>(listener: L, mut c: Config<R>) -> io::Result<Self>
    where
        R: io::BufRead,
    {
        use rustls::server::{AllowAnyAnonymousOrAuthenticatedClient, AllowAnyAuthenticatedClient};
        use rustls::server::{NoClientAuth, ServerConfig, ServerSessionMemoryCache};

        let cert_chain = load_certs(&mut c.cert_chain)
            .map_err(|e| io::Error::new(e.kind(), format!("bad TLS cert chain: {}", e)))?;

        let key = load_private_key(&mut c.private_key)
            .map_err(|e| io::Error::new(e.kind(), format!("bad TLS private key: {}", e)))?;

        let client_auth = match c.ca_certs {
            Some(ref mut ca_certs) => match load_ca_certs(ca_certs) {
                Ok(ca_roots) if c.mandatory_mtls => AllowAnyAuthenticatedClient::new(ca_roots),
                Ok(ca_roots) => AllowAnyAnonymousOrAuthenticatedClient::new(ca_roots),
                Err(e) => return Err(io::Error::new(e.kind(), format!("bad CA cert(s): {}", e))),
            },
            None => NoClientAuth::new(),
        };

        let mut tls_config = ServerConfig::builder()
            .with_cipher_suites(&c.ciphersuites)
            .with_safe_default_kx_groups()
            .with_safe_default_protocol_versions()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("bad TLS config: {}", e)))?
            .with_client_cert_verifier(client_auth)
            .with_single_cert(cert_chain, key)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("bad TLS config: {}", e)))?;

        tls_config.ignore_client_order = c.prefer_server_order;

        tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        if cfg!(feature = "http2") {
            tls_config.alpn_protocols.insert(0, b"h2".to_vec());
        }

        tls_config.session_storage = ServerSessionMemoryCache::new(1024);
        tls_config.ticketer = rustls::Ticketer::new().map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("bad TLS ticketer: {}", e))
        })?;

        let acceptor = TlsAcceptor::from(Arc::new(tls_config));
        Ok(TlsListener { listener, acceptor })
    }
}

impl<L> Listener for TlsListener<L>
where
    L: Listener + Unpin,
    L::Connection: Unpin,
{
    type Connection = TlsStream<L::Connection>;

    fn local_addr(&self) -> Option<BindableAddr> {
        self.listener.local_addr()
    }

    fn poll_accept(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<Self::Connection>> {
        match futures::ready!(Pin::new(&mut self.listener).poll_accept(cx)) {
            Ok(conn) => Poll::Ready(Ok(TlsStream {
                remote: conn.peer_address(),
                state: TlsState::Handshaking(self.acceptor.accept(conn)),
                // These are empty and filled in after handshake is complete.
                certs: Certificates::default(),
            })),
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl<C: Connection + Unpin> Connection for TlsStream<C> {
    fn peer_address(&self) -> Option<BindableAddr> {
        self.remote.clone()
    }

    fn enable_nodelay(&self) -> io::Result<()> {
        // If `Handshaking` is `None`, it either failed, so we returned an `Err`
        // from `poll_accept()` and there's no connection to enable `NODELAY`
        // on, or it succeeded, so we're in the `Streaming` stage and we have
        // infallible access to the connection.
        match &self.state {
            TlsState::Handshaking(accept) => match accept.get_ref() {
                None => Ok(()),
                Some(s) => s.enable_nodelay(),
            },
            TlsState::Streaming(stream) => stream.get_ref().0.enable_nodelay(),
        }
    }

    fn peer_certificates(&self) -> Option<Certificates> {
        Some(self.certs.clone())
    }
}

impl<C: Connection + Unpin> TlsStream<C> {
    fn poll_accept_then<F, T>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut f: F,
    ) -> Poll<io::Result<T>>
    where
        F: FnMut(&mut BareTlsStream<C>, &mut Context<'_>) -> Poll<io::Result<T>>,
    {
        loop {
            match self.state {
                TlsState::Handshaking(ref mut accept) => {
                    match futures::ready!(Pin::new(accept).poll(cx)) {
                        Ok(stream) => {
                            if let Some(cert_chain) = stream.get_ref().1.peer_certificates() {
                                self.certs.set(cert_chain.to_vec());
                            }

                            self.state = TlsState::Streaming(stream);
                        }
                        Err(e) => {
                            log::warn!("tls handshake with {:?} failed: {}", self.remote, e);
                            return Poll::Ready(Err(e));
                        }
                    }
                }
                TlsState::Streaming(ref mut stream) => return f(stream, cx),
            }
        }
    }
}

impl<C: Connection + Unpin> AsyncRead for TlsStream<C> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.poll_accept_then(cx, |stream, cx| Pin::new(stream).poll_read(cx, buf))
    }
}

impl<C: Connection + Unpin> AsyncWrite for TlsStream<C> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_accept_then(cx, |stream, cx| Pin::new(stream).poll_write(cx, buf))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            TlsState::Handshaking(accept) => match accept.get_mut() {
                Some(io) => Pin::new(io).poll_flush(cx),
                None => Poll::Ready(Ok(())),
            },
            TlsState::Streaming(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.state {
            TlsState::Handshaking(accept) => match accept.get_mut() {
                Some(io) => Pin::new(io).poll_shutdown(cx),
                None => Poll::Ready(Ok(())),
            },
            TlsState::Streaming(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}
