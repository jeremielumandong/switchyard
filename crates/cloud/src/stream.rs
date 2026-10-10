//! Object bodies as `AsyncRead` / `AsyncWrite` streams for the transfer queue.
//!
//! Downloads pipe the HTTP body through a bounded in-memory pipe; uploads collect parts of
//! [`PART_SIZE`] and send a small file in one request or a large one as a multipart upload
//! (S3) / block list (Azure). Errors on the far side of the pipe come back through a
//! channel, so a failed download never looks like a short file and `shutdown` returns only
//! once the object exists. Dropping a writer without `shutdown` (cancel) abandons the upload.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use switchyard_remote::{FsReader, FsWriter};
use tokio::io::{
    AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, DuplexStream, ReadBuf,
};
use tokio::sync::oneshot;

use crate::error::Result;

/// Upload part size: S3's minimum part is 5 MiB; 8 MiB keeps memory low and requests few.
pub(crate) const PART_SIZE: usize = 8 * 1024 * 1024;
const PIPE: usize = 256 * 1024;

fn io_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// Poll the far side's outcome after the pipe ended.
fn poll_done(
    done: &mut Option<oneshot::Receiver<io::Result<()>>>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    let Some(rx) = done.as_mut() else {
        return Poll::Ready(Ok(()));
    };
    match Pin::new(rx).poll(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(r) => {
            *done = None;
            Poll::Ready(match r {
                Ok(r) => r,
                Err(_) => Err(io_err("the transfer stopped unexpectedly")),
            })
        }
    }
}

use std::future::Future as _;

struct PipeReader {
    inner: DuplexStream,
    done: Option<oneshot::Receiver<io::Result<()>>>,
}

impl AsyncRead for PipeReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) if buf.filled().len() == before => {
                // End of the pipe: a clean end or the download's error.
                poll_done(&mut self.done, cx)
            }
            other => other,
        }
    }
}

/// Stream an HTTP response body as a reader.
pub(crate) fn body_reader(mut resp: reqwest::Response) -> FsReader {
    let (mut tx, rx) = tokio::io::duplex(PIPE);
    let (done_tx, done_rx) = oneshot::channel();
    tokio::spawn(async move {
        let r = async {
            while let Some(chunk) = resp.chunk().await.map_err(io_err)? {
                tx.write_all(&chunk).await?;
            }
            tx.shutdown().await
        }
        .await;
        drop(tx);
        let _ = done_tx.send(r);
    });
    Box::new(PipeReader {
        inner: rx,
        done: Some(done_rx),
    })
}

/// Where an upload's bytes go.
pub(crate) trait ChunkSink: Send + Sync + 'static {
    /// Store a whole (small) object in one request.
    fn put_whole(&self, data: Vec<u8>) -> BoxFuture<'_, Result<()>>;
    /// Start a multipart upload; returns its id.
    fn begin(&self) -> BoxFuture<'_, Result<String>>;
    /// Upload part `n` (from 1); returns what [`ChunkSink::complete`] needs for it.
    fn put_part<'a>(
        &'a self,
        upload: &'a str,
        n: u32,
        data: Vec<u8>,
    ) -> BoxFuture<'a, Result<String>>;
    /// Commit the parts in order.
    fn complete<'a>(&'a self, upload: &'a str, parts: Vec<String>) -> BoxFuture<'a, Result<()>>;
    /// Abandon the upload.
    fn abort<'a>(&'a self, upload: &'a str) -> BoxFuture<'a, Result<()>>;
}

struct PipeWriter {
    inner: DuplexStream,
    committed: Arc<AtomicBool>,
    done: Option<oneshot::Receiver<io::Result<()>>>,
}

impl AsyncWrite for PipeWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            // The uploader gave up: report why.
            Poll::Ready(Err(_)) => match poll_done(&mut self.done, cx) {
                Poll::Ready(Ok(())) => Poll::Ready(Err(io_err("the upload ended early"))),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            },
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.committed.store(true, Ordering::SeqCst);
        match Pin::new(&mut self.inner).poll_shutdown(cx) {
            Poll::Ready(_) => poll_done(&mut self.done, cx),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Read up to `PART_SIZE` bytes; fewer only at the end of the stream.
async fn read_part(r: &mut DuplexStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(PART_SIZE);
    while buf.len() < PART_SIZE {
        let n = (&mut *r)
            .take((PART_SIZE - buf.len()) as u64)
            .read_to_end(&mut buf)
            .await?;
        if n == 0 {
            break;
        }
    }
    Ok(buf)
}

async fn upload(
    sink: Arc<dyn ChunkSink>,
    mut rx: DuplexStream,
    committed: Arc<AtomicBool>,
) -> io::Result<()> {
    let first = read_part(&mut rx).await?;
    if first.len() < PART_SIZE {
        if !committed.load(Ordering::SeqCst) {
            return Ok(());
        }
        return sink.put_whole(first).await.map_err(io_err);
    }
    let id = sink.begin().await.map_err(io_err)?;
    let result = async {
        let mut parts = vec![sink.put_part(&id, 1, first).await.map_err(io_err)?];
        loop {
            let next = read_part(&mut rx).await?;
            if next.is_empty() {
                break;
            }
            let n = parts.len() as u32 + 1;
            let last = next.len() < PART_SIZE;
            parts.push(sink.put_part(&id, n, next).await.map_err(io_err)?);
            if last {
                break;
            }
        }
        // Wait for the writer to finish or go away before deciding.
        let mut rest = Vec::new();
        rx.read_to_end(&mut rest).await?;
        if !rest.is_empty() {
            return Err(io_err("data arrived after the last part"));
        }
        if !committed.load(Ordering::SeqCst) {
            return Err(io_err("cancelled"));
        }
        sink.complete(&id, parts).await.map_err(io_err)
    }
    .await;
    if result.is_err() {
        let _ = sink.abort(&id).await;
    }
    result
}

/// A writer whose bytes go to `sink`.
pub(crate) fn upload_writer(sink: Arc<dyn ChunkSink>) -> FsWriter {
    let (tx, rx) = tokio::io::duplex(PIPE);
    let (done_tx, done_rx) = oneshot::channel();
    let committed = Arc::new(AtomicBool::new(false));
    let c = committed.clone();
    tokio::spawn(async move {
        let r = upload(sink, rx, c).await;
        let _ = done_tx.send(r);
    });
    Box::new(PipeWriter {
        inner: tx,
        committed,
        done: Some(done_rx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Memory {
        log: Mutex<Vec<String>>,
        object: Mutex<Vec<u8>>,
        parts: Mutex<Vec<Vec<u8>>>,
    }

    impl ChunkSink for Memory {
        fn put_whole(&self, data: Vec<u8>) -> BoxFuture<'_, Result<()>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("whole {}", data.len()));
                *self.object.lock().unwrap() = data;
                Ok(())
            })
        }
        fn begin(&self) -> BoxFuture<'_, Result<String>> {
            Box::pin(async move {
                self.log.lock().unwrap().push("begin".into());
                Ok("u1".into())
            })
        }
        fn put_part<'a>(
            &'a self,
            _upload: &'a str,
            n: u32,
            data: Vec<u8>,
        ) -> BoxFuture<'a, Result<String>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("part {n} {}", data.len()));
                self.parts.lock().unwrap().push(data);
                Ok(format!("e{n}"))
            })
        }
        fn complete<'a>(&'a self, _: &'a str, parts: Vec<String>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("complete {}", parts.join(",")));
                *self.object.lock().unwrap() = self.parts.lock().unwrap().concat();
                Ok(())
            })
        }
        fn abort<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                self.log.lock().unwrap().push("abort".into());
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn small_upload_is_one_request() {
        let sink = Arc::new(Memory::default());
        let mut w = upload_writer(sink.clone());
        w.write_all(b"hello").await.unwrap();
        w.shutdown().await.unwrap();
        assert_eq!(*sink.log.lock().unwrap(), ["whole 5"]);
        assert_eq!(*sink.object.lock().unwrap(), b"hello");
    }

    #[tokio::test]
    async fn large_upload_uses_parts() {
        let sink = Arc::new(Memory::default());
        let mut w = upload_writer(sink.clone());
        let data: Vec<u8> = (0..PART_SIZE * 2 + 10).map(|i| (i % 251) as u8).collect();
        for c in data.chunks(100_000) {
            w.write_all(c).await.unwrap();
        }
        w.shutdown().await.unwrap();
        assert_eq!(
            *sink.log.lock().unwrap(),
            [
                "begin".to_owned(),
                format!("part 1 {PART_SIZE}"),
                format!("part 2 {PART_SIZE}"),
                "part 3 10".into(),
                "complete e1,e2,e3".into()
            ]
        );
        assert!(*sink.object.lock().unwrap() == data);
    }

    #[tokio::test]
    async fn exact_multiple_of_part_size() {
        let sink = Arc::new(Memory::default());
        let mut w = upload_writer(sink.clone());
        w.write_all(&vec![1u8; PART_SIZE]).await.unwrap();
        w.shutdown().await.unwrap();
        assert_eq!(
            *sink.log.lock().unwrap(),
            [
                "begin".to_owned(),
                format!("part 1 {PART_SIZE}"),
                "complete e1".into()
            ]
        );
    }

    #[tokio::test]
    async fn dropped_writer_stores_nothing() {
        let sink = Arc::new(Memory::default());
        let mut w = upload_writer(sink.clone());
        w.write_all(&vec![1u8; PART_SIZE + 5]).await.unwrap();
        drop(w);
        for _ in 0..50 {
            if sink.log.lock().unwrap().last().map(String::as_str) == Some("abort") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let log = sink.log.lock().unwrap().clone();
        assert_eq!(log.last().map(String::as_str), Some("abort"), "{log:?}");
        assert!(sink.object.lock().unwrap().is_empty());

        let sink = Arc::new(Memory::default());
        let mut w = upload_writer(sink.clone());
        w.write_all(b"tiny").await.unwrap();
        drop(w);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(sink.log.lock().unwrap().is_empty());
    }
}
