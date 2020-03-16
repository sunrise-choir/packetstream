use byteorder::{BigEndian, ByteOrder};
use core::pin::Pin;
use core::task::{Context, Poll, Poll::Pending, Poll::Ready};
use futures::io::{AsyncRead, AsyncReadExt};
use futures::stream::{FusedStream, Stream};
use std::mem::replace;

use crate::packet::*;
use crate::PinFut;
use snafu::{ensure, ResultExt, Snafu};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to receive packet: {}", source))]
    Recv { source: std::io::Error },

    #[snafu(display("IO error while reading packet header: {}", source))]
    Header { source: std::io::Error },

    #[snafu(display(
        "IO error while reading packet body. Body size: {}. Error: {}",
        size,
        source
    ))]
    Body { size: usize, source: std::io::Error },

    #[snafu(display("PacketStream underlying reader closed without goodbye"))]
    NoGoodbye {},
}

async fn recv<R>(r: &mut R) -> Result<Option<Packet>, Error>
where
    R: AsyncRead + Unpin,
{
    let mut head = [0; 9];
    let n = r.read(&mut head).await.context(Header)?;
    ensure!(n != 0, NoGoodbye);
    if n < head.len() {
        r.read_exact(&mut head[n..]).await.context(Header)?;
    }

    if head == [0u8; 9] {
        return Ok(None); // RPC goodbye
    }

    let body_len = BigEndian::read_u32(&head[1..5]) as usize;
    let id = BigEndian::read_i32(&head[5..]);

    let mut body = vec![0; body_len];
    r.read_exact(&mut body)
        .await
        .context(Body { size: body_len })?;

    Ok(Some(Packet::new(
        head[0].into(),
        head[0].into(),
        head[0].into(),
        id,
        body,
    )))
}

async fn recv_move<R>(mut r: R) -> (R, Result<Option<Packet>, Error>)
where
    R: AsyncRead + Unpin + 'static,
{
    let res = recv(&mut r).await;
    (r, res)
}

/// # Examples
/// ```rust
/// use futures::executor::block_on;
/// use futures::sink::SinkExt;
/// use futures::stream::TryStreamExt;
/// use ssb_packetstream::*;
///
/// let p = Packet::new(IsStream::Yes,
///                     IsEnd::No,
///                     BodyType::Binary,
///                     12345,
///                     vec![1,2,3,4,5]);
///
/// let (writer, reader) = async_ringbuffer::ring_buffer(64);
///
/// let mut sink = PacketSink::new(writer);
/// let mut stream = PacketStream::new(reader);
/// block_on(async {
///     sink.send(p).await;
///     let r = stream.try_next().await.unwrap().unwrap();
///     assert_eq!(&r.body, &[1,2,3,4,5]);
///     assert_eq!(r.id, 12345);
/// });
/// ```
pub struct PacketStream<R> {
    state: State<R>,
}
impl<R> PacketStream<R> {
    pub fn new(r: R) -> PacketStream<R> {
        PacketStream {
            state: State::Ready(r),
        }
    }

    pub fn is_closed(&self) -> bool {
        match &self.state {
            State::Closed(_) => true,
            _ => false,
        }
    }

    pub fn into_inner(mut self) -> R {
        match self.state.take() {
            State::Ready(r) | State::Closed(r) => r,
            _ => panic!(),
        }
    }
}

enum State<R> {
    Ready(R),
    Waiting(PinFut<(R, Result<Option<Packet>, Error>)>),
    Closed(R),
    Invalid,
}
impl<R> State<R> {
    fn take(&mut self) -> Self {
        replace(self, State::Invalid)
    }
}

fn next<R>(state: State<R>, cx: &mut Context) -> (State<R>, Poll<Option<Result<Packet, Error>>>)
where
    R: AsyncRead + Unpin + 'static,
{
    match state {
        State::Ready(r) => next(State::Waiting(Box::pin(recv_move(r))), cx),
        State::Waiting(mut f) => match f.as_mut().poll(cx) {
            Pending => (State::Waiting(f), Pending),
            Ready((r, Ok(None))) => (State::Closed(r), Ready(None)),
            Ready((r, Err(e))) => (State::Closed(r), Ready(Some(Err(e)))),
            Ready((r, res)) => (State::Ready(r), Ready(res.transpose())),
        },
        State::Closed(r) => (State::Closed(r), Ready(None)),
        State::Invalid => panic!(),
    }
}

impl<R: AsyncRead + Unpin + 'static> Stream for PacketStream<R> {
    type Item = Result<Packet, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (state, poll) = next(self.state.take(), cx);
        self.state = state;
        poll
    }
}

impl<R: AsyncRead + Unpin + 'static> FusedStream for PacketStream<R> {
    fn is_terminated(&self) -> bool {
        self.is_closed()
    }
}
