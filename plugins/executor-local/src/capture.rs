//! Capped capture of a child's output pipe.

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

/// Keeps at most `cap` bytes of a stream: the **tail**, since the end of a command's
/// output (the error, the test summary) is usually what matters.
#[derive(Debug)]
pub(crate) struct TailBuffer {
    cap: usize,
    data: Vec<u8>,
    /// Bytes dropped from the front so far.
    dropped: usize,
}

impl TailBuffer {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            cap,
            data: Vec::new(),
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        // Compact lazily so we don't shift the buffer on every read.
        if self.data.len() > self.cap.saturating_mul(2).max(64 * 1024) {
            self.compact();
        }
    }

    fn compact(&mut self) {
        if self.data.len() > self.cap {
            let excess = self.data.len() - self.cap;
            self.data.drain(..excess);
            self.dropped += excess;
        }
    }

    /// The captured text (lossy UTF-8), prefixed with a marker if anything was dropped.
    pub(crate) fn into_string(mut self) -> String {
        self.compact();
        let mut start = 0;
        if self.dropped > 0 {
            // Don't start in the middle of a UTF-8 sequence.
            while start < self.data.len() && start < 4 && (self.data[start] & 0xC0) == 0x80 {
                start += 1;
            }
        }
        let text = String::from_utf8_lossy(&self.data[start..]);
        if self.dropped > 0 {
            format!(
                "[... {} bytes of earlier output dropped ...]\n{text}",
                self.dropped + start
            )
        } else {
            text.into_owned()
        }
    }
}

/// Reads `reader` to EOF (or until `stop` is cancelled) into a [`TailBuffer`].
///
/// `stop` exists for background processes that inherit the pipe and keep it open after
/// the main process has exited.
pub(crate) async fn read_capped<R>(mut reader: R, cap: usize, stop: CancellationToken) -> TailBuffer
where
    R: AsyncRead + Unpin,
{
    let mut out = TailBuffer::new(cap);
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        tokio::select! {
            biased;
            read = reader.read(&mut chunk) => match read {
                Ok(0) => break,
                Ok(n) => out.push(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::debug!("executor-local: pipe read failed: {e}");
                    break;
                }
            },
            _ = stop.cancelled() => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_is_kept_verbatim() {
        let mut b = TailBuffer::new(10);
        b.push(b"hello");
        assert_eq!(b.into_string(), "hello");
    }

    #[test]
    fn overflow_keeps_the_tail() {
        let mut b = TailBuffer::new(4);
        b.push(b"abcdefgh");
        let s = b.into_string();
        assert!(s.ends_with("efgh"), "{s}");
        assert!(s.contains("4 bytes of earlier output dropped"), "{s}");
    }

    #[test]
    fn overflow_does_not_split_utf8() {
        let mut b = TailBuffer::new(3);
        b.push("xé€".as_bytes()); // € is 3 bytes, é is 2
        let s = b.into_string();
        assert!(s.ends_with('€'), "{s}");
        assert!(!s.contains('\u{FFFD}'), "{s}");
    }
}
