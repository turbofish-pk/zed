use std::str;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt as _, SinkExt as _,
    channel::mpsc::{Receiver, Sender, channel},
    io::BufReader,
};
use gpui::{BackgroundExecutor, Task};
use log::warn;
use parking_lot::Mutex;

use crate::{
    AnyResponse, CONTENT_LEN_HEADER, IoHandler, IoKind, NotificationOrRequest, RequestId,
    ResponseHandler,
};

const HEADER_DELIMITER: &[u8; 4] = b"\r\n\r\n";

/// Bounds the number of incoming LSP messages buffered between the background
/// reader and the foreground dispatcher. When the queue is full, the reader
/// stops reading the server's stdout, letting the OS pipe apply backpressure
/// to the server instead of buffering messages in memory without limit while
/// the foreground thread is unresponsive.
pub(crate) const INCOMING_MESSAGE_QUEUE_CAPACITY: usize = 128;

/// Handler for stdout of language server.
pub struct LspStdoutHandler {
    pub(super) loop_handle: Task<Result<()>>,
    pub(super) incoming_messages: Receiver<NotificationOrRequest>,
}

async fn read_headers<Stdout>(reader: &mut BufReader<Stdout>, buffer: &mut Vec<u8>) -> Result<()>
where
    Stdout: AsyncRead + Unpin + Send + 'static,
{
    loop {
        if buffer.len() >= HEADER_DELIMITER.len()
            && buffer[(buffer.len() - HEADER_DELIMITER.len())..] == HEADER_DELIMITER[..]
        {
            return Ok(());
        }

        if reader.read_until(b'\n', buffer).await? == 0 {
            anyhow::bail!("cannot read LSP message headers");
        }
    }
}

impl LspStdoutHandler {
    pub fn new<Input>(
        stdout: Input,
        response_handlers: Arc<Mutex<Option<HashMap<RequestId, ResponseHandler>>>>,
        io_handlers: Arc<Mutex<HashMap<i32, IoHandler>>>,
        cx: BackgroundExecutor,
    ) -> Self
    where
        Input: AsyncRead + Unpin + Send + 'static,
    {
        let (tx, notifications_channel) = channel(INCOMING_MESSAGE_QUEUE_CAPACITY);
        let loop_handle = cx.spawn(Self::handler(stdout, tx, response_handlers, io_handlers));
        Self {
            loop_handle,
            incoming_messages: notifications_channel,
        }
    }

    async fn handler<Input>(
        stdout: Input,
        mut notifications_sender: Sender<NotificationOrRequest>,
        response_handlers: Arc<Mutex<Option<HashMap<RequestId, ResponseHandler>>>>,
        io_handlers: Arc<Mutex<HashMap<i32, IoHandler>>>,
    ) -> anyhow::Result<()>
    where
        Input: AsyncRead + Unpin + Send + 'static,
    {
        let mut stdout = BufReader::new(stdout);

        let mut buffer = Vec::new();

        loop {
            buffer.clear();

            read_headers(&mut stdout, &mut buffer).await?;

            let headers = std::str::from_utf8(&buffer)?;

            let message_len = headers
                .split('\n')
                .find(|line| line.starts_with(CONTENT_LEN_HEADER))
                .and_then(|line| line.strip_prefix(CONTENT_LEN_HEADER))
                .with_context(|| format!("invalid LSP message header {headers:?}"))?
                .trim_end()
                .parse()?;

            buffer.resize(message_len, 0);
            stdout.read_exact(&mut buffer).await?;

            if let Ok(message) = str::from_utf8(&buffer) {
                log::trace!("incoming message: {message}");
                for handler in io_handlers.lock().values_mut() {
                    handler(IoKind::StdOut, message);
                }
            }

            if let Ok(msg) = serde_json::from_slice::<NotificationOrRequest>(&buffer) {
                notifications_sender.send(msg).await?;
            } else if let Ok(AnyResponse {
                id, error, result, ..
            }) = serde_json::from_slice(&buffer)
            {
                let handler = {
                    response_handlers
                        .lock()
                        .as_mut()
                        .and_then(|handlers| handlers.remove(&id))
                };
                if let Some(handler) = handler {
                    if let Some(error) = error {
                        handler(Err(error)).await;
                    } else if let Some(result) = result {
                        handler(Ok(result.get().into())).await;
                    } else {
                        handler(Ok("null".into())).await;
                    }
                }
            } else {
                warn!(
                    "failed to deserialize LSP message:\n{}",
                    std::str::from_utf8(&buffer)?
                );
            }
        }
    }
}

