//! A driver that runs a [`Handshake`] over [`futures`] I/O traits.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

use crate::{EstablishError, Handshake, ProxySpec, Step, Target};

/// Runs the proxy handshake over `stream` and returns the tunneled stream.
///
/// The stream must already be connected to the proxy (and wrapped in TLS if
/// [`ProxySpec::tls`] asks for it). See [`Handshake::new`] for how `target`
/// interacts with DNS resolution.
pub async fn establish<S>(
    mut stream: S,
    spec: &ProxySpec,
    target: &Target,
) -> Result<Tunneled<S>, EstablishError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut handshake = Handshake::new(spec, target)?;
    let mut buffer = [0u8; 4096];
    let mut received_length = 0;
    loop {
        let step = handshake.advance(&buffer[..received_length])?;
        received_length = 0;
        match step {
            Step::Send(bytes) => {
                stream.write_all(&bytes).await?;
                stream.flush().await?;
            }
            Step::NeedMoreInput => {
                received_length = stream.read(&mut buffer).await?;
                if received_length == 0 {
                    return Err(EstablishError::Io(io::ErrorKind::UnexpectedEof.into()));
                }
            }
            Step::Done { leftover } => {
                return Ok(Tunneled {
                    leftover,
                    offset: 0,
                    stream,
                });
            }
        }
    }
}

/// A tunneled stream returned by [`establish`]: replays the handshake's
/// leftover bytes before reading from the underlying transport. Writes pass
/// straight through.
pub struct Tunneled<S> {
    leftover: Vec<u8>,
    offset: usize,
    stream: S,
}

impl<S: AsyncRead + Unpin> AsyncRead for Tunneled<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.offset < this.leftover.len() {
            let remaining = &this.leftover[this.offset..];
            let length = remaining.len().min(buf.len());
            buf[..length].copy_from_slice(&remaining[..length]);
            this.offset += length;
            if this.offset == this.leftover.len() {
                this.leftover = Vec::new();
                this.offset = 0;
            }
            return Poll::Ready(Ok(length));
        }
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tunneled<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_close(cx)
    }
}

