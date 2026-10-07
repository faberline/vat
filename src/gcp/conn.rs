//! One connection type for both guest uplinks and host TCP clients, and a
//! channel-fed listener so axum and tonic servers can take connections that
//! the VMM accepted elsewhere.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

/// A client stream and the address of its caller (a pod or container IP for
/// guest uplinks, loopback for host clients).
pub struct Conn {
    io: Box<dyn Io>,
    pub peer: IpAddr,
}

impl Conn {
    pub fn new(io: impl Io, peer: IpAddr) -> Self {
        Self {
            io: Box::new(io),
            peer,
        }
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.io).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.io).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.io).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

impl tonic::transport::server::Connected for Conn {
    type ConnectInfo = Peer;
    fn connect_info(&self) -> Peer {
        Peer(self.peer)
    }
}

/// The caller's IP, available to axum handlers as `ConnectInfo<Peer>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer(pub IpAddr);

/// Where a service's connections arrive.
pub type Inbox = mpsc::Sender<Conn>;

/// An axum listener fed by a channel. TLS (when configured) is terminated in
/// per-connection tasks so one slow handshake does not stall accepts.
pub struct ChanListener<T> {
    rx: mpsc::Receiver<(T, Peer)>,
}

impl<T: Io> axum::serve::Listener for ChanListener<T> {
    type Io = T;
    type Addr = Peer;
    async fn accept(&mut self) -> (T, Peer) {
        match self.rx.recv().await {
            Some(next) => next,
            // Every sender is gone: the VMM is shutting down.
            None => std::future::pending().await,
        }
    }
    fn local_addr(&self) -> io::Result<Peer> {
        Ok(Peer(IpAddr::V4(Ipv4Addr::LOCALHOST)))
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, ChanListener<Conn>>>
    for Peer
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, ChanListener<Conn>>) -> Self {
        *stream.remote_addr()
    }
}

/// A plain listener: connections are served as they arrive.
pub fn plain(capacity: usize) -> (Inbox, ChanListener<Conn>) {
    let (tx, mut rx) = mpsc::channel::<Conn>(capacity);
    let (out_tx, out_rx) = mpsc::channel(capacity);
    tokio::spawn(async move {
        while let Some(conn) = rx.recv().await {
            let peer = Peer(conn.peer);
            if out_tx.send((conn, peer)).await.is_err() {
                break;
            }
        }
    });
    (tx, ChanListener { rx: out_rx })
}

/// A TLS listener: each connection's handshake runs in its own task.
pub fn tls(
    capacity: usize,
    config: Arc<rustls::ServerConfig>,
) -> (Inbox, ChanListener<tokio_rustls::server::TlsStream<Conn>>) {
    let (tx, mut rx) = mpsc::channel::<Conn>(capacity);
    let (out_tx, out_rx) = mpsc::channel(capacity);
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    tokio::spawn(async move {
        while let Some(conn) = rx.recv().await {
            let peer = Peer(conn.peer);
            let acceptor = acceptor.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                match acceptor.accept(conn).await {
                    Ok(stream) => {
                        let _ = out_tx.send((stream, peer)).await;
                    }
                    Err(err) => eprintln!("gcp: TLS handshake from {}: {err}", peer.0),
                }
            });
        }
    });
    (tx, ChanListener { rx: out_rx })
}

/// A stream of connections for tonic's `serve_with_incoming`.
pub fn incoming(
    capacity: usize,
) -> (
    Inbox,
    impl tokio_stream::Stream<Item = Result<Conn, io::Error>>,
) {
    let (tx, rx) = mpsc::channel::<Conn>(capacity);
    let stream = tokio_stream::StreamExt::map(tokio_stream::wrappers::ReceiverStream::new(rx), Ok);
    (tx, stream)
}

/// Accept host TCP clients into `inbox`.
pub async fn accept_tcp(listener: tokio::net::TcpListener, inbox: Inbox) {
    loop {
        let Ok((stream, addr)) = listener.accept().await else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        if inbox.send(Conn::new(stream, addr.ip())).await.is_err() {
            return;
        }
    }
}

/// Parse the peer address carried in an uplink header.
pub fn parse_peer(peer: &str) -> IpAddr {
    peer.parse::<IpAddr>()
        .or_else(|_| peer.parse::<SocketAddr>().map(|a| a.ip()))
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
}
