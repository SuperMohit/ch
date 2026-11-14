//! One plugin's JSON-RPC connection over its stdin and stdout.
//!
//! A reader task owns stdout: it routes responses to the calls waiting for them and
//! hands notifications to a callback. A writer task owns stdin, so a write blocked on
//! a full pipe holds no lock anyone else needs; stopping aborts it, which closes stdin.
//! When stdout ends, every pending call fails at once.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ch_plugin_protocol::{CancelParams, Id, MAX_MESSAGE, Message, RpcError, method};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Why a call got no result.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// The plugin answered with a JSON-RPC error.
    Rpc(RpcError),
    Timeout(Duration),
    Cancelled,
    /// The connection is closed: the plugin exited or was stopped.
    Closed,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "the plugin returned an error: {}", e.message),
            Self::Timeout(d) => write!(f, "no answer within {} s", d.as_secs()),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Closed => f.write_str("the plugin isn't running"),
        }
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// A notification from the plugin: `(method, params)`.
pub type OnNotify = Box<dyn Fn(&str, Value) + Send + Sync>;

pub struct Conn {
    next_id: AtomicI64,
    pending: Pending,
    out: mpsc::Sender<Vec<u8>>,
    closed: Arc<AtomicBool>,
    /// Cancelled when the reader sees the end of stdout.
    pub ended: CancellationToken,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Conn {
    /// Starts the reader and writer tasks. `name` is the plugin's, for logs.
    pub fn new(
        read: impl AsyncRead + Unpin + Send + 'static,
        write: impl AsyncWrite + Unpin + Send + 'static,
        name: &str,
        on_notify: OnNotify,
    ) -> Self {
        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let ended = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(256);
        let writer = tokio::spawn({
            let pending = pending.clone();
            let closed = closed.clone();
            let ended = ended.clone();
            async move {
                let mut write = write;
                while let Some(line) = rx.recv().await {
                    if write.write_all(&line).await.is_err() || write.flush().await.is_err() {
                        // The plugin closed its stdin: nothing more can be asked of it.
                        shut(&closed, &pending, &ended);
                        return;
                    }
                }
                let _ = write.shutdown().await;
            }
        });
        let reader = tokio::spawn({
            let pending = pending.clone();
            let closed = closed.clone();
            let ended = ended.clone();
            let tx = tx.clone();
            let name = name.to_string();
            async move {
                read_loop(read, &pending, &tx, &on_notify, &name).await;
                shut(&closed, &pending, &ended);
            }
        });
        Self {
            next_id: AtomicI64::new(1),
            pending,
            out: tx,
            closed,
            ended,
            reader,
            writer,
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Sends a request and waits up to `timeout` for its result, or until `cancel`. A
    /// call that is given up sends `$/cancel`.
    pub async fn call(
        &self,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            // Checked under the lock `shut` clears under, so a call can't slip in after
            // the clear and wait for its timeout.
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            if self.is_closed() {
                return Err(CallError::Closed);
            }
            pending.insert(id, tx);
        }
        let line = Message::request(id, method, params).to_line();
        let outcome = tokio::select! {
            r = tokio::time::timeout(timeout, async {
                self.out.send(line).await.map_err(|_| CallError::Closed)?;
                match rx.await {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(CallError::Rpc(e)),
                    Err(_) => Err(CallError::Closed),
                }
            }) => r.unwrap_or(Err(CallError::Timeout(timeout))),
            _ = cancel.cancelled() => Err(CallError::Cancelled),
        };
        if matches!(outcome, Err(CallError::Timeout(_) | CallError::Cancelled)) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            self.notify(method::CANCEL, CancelParams { id: Id::Number(id) });
        }
        outcome
    }

    /// Sends a notification without waiting. Dropped if the plugin's queue is full or
    /// the connection is closed.
    pub fn notify(&self, method: &str, params: impl Serialize) {
        if !self.is_closed() {
            let _ = self
                .out
                .try_send(Message::notification(method, params).to_line());
        }
    }

    /// Stops both tasks; stdin closes with the writer.
    pub fn close(&self) {
        self.writer.abort();
        self.reader.abort();
        shut(&self.closed, &self.pending, &self.ended);
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.close();
    }
}

/// Marks the connection closed and fails every pending call (dropping its sender).
fn shut(closed: &AtomicBool, pending: &Pending, ended: &CancellationToken) {
    let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
    closed.store(true, Ordering::SeqCst);
    pending.clear();
    drop(pending);
    ended.cancel();
}

/// The id of an oversized response, from its first bytes. Only the shape ch's own SDK
/// and the protocol's examples write (`{"jsonrpc":"2.0","id":N,...`), so another key's
/// value is never mistaken for it.
fn oversized_id(head: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(&head[..head.len().min(128)]).ok()?;
    let rest = text.trim_start().strip_prefix('{')?.trim_start();
    let rest = match rest.strip_prefix("\"jsonrpc\"") {
        Some(r) => {
            let r = r.trim_start().strip_prefix(':')?.trim_start();
            r.strip_prefix("\"2.0\"")?
                .trim_start()
                .strip_prefix(',')?
                .trim_start()
        }
        None => rest,
    };
    let rest = rest
        .strip_prefix("\"id\"")?
        .trim_start()
        .strip_prefix(':')?
        .trim_start();
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    let after = rest[digits.len()..].trim_start();
    if !after.starts_with(',') {
        return None;
    }
    digits.parse().ok()
}

async fn read_loop(
    read: impl AsyncRead + Unpin,
    pending: &Pending,
    tx: &mpsc::Sender<Vec<u8>>,
    on_notify: &OnNotify,
    name: &str,
) {
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        // `take` bounds the line; one longer than the cap is skipped to its end.
        let n = match (&mut read)
            .take(MAX_MESSAGE as u64 + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        if n > MAX_MESSAGE && !line.ends_with(b"\n") {
            tracing::warn!(plugin = name, "skipped a message over {MAX_MESSAGE} bytes");
            // A response that is too big fails its call at once.
            if let Some(id) = oversized_id(&line) {
                let waiting = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(waiting) = waiting {
                    let _ = waiting.send(Err(RpcError::new(
                        ch_plugin_protocol::code::INTERNAL_ERROR,
                        format!(
                            "the plugin's answer was over {} MB",
                            MAX_MESSAGE / (1024 * 1024)
                        ),
                    )));
                }
            }
            let mut rest = Vec::new();
            loop {
                rest.clear();
                match (&mut read)
                    .take(64 * 1024)
                    .read_until(b'\n', &mut rest)
                    .await
                {
                    Ok(0) | Err(_) => return,
                    Ok(_) if rest.ends_with(b"\n") => break,
                    Ok(_) => {}
                }
            }
            continue;
        }
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        if text.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match Message::parse(text) {
            Ok(Message::Response {
                id: Id::Number(id),
                result,
            }) => {
                let waiting = pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(waiting) = waiting {
                    let _ = waiting.send(result);
                }
            }
            Ok(Message::Response { .. }) => {}
            Ok(Message::Notification { method, params }) => on_notify(&method, params),
            Ok(Message::Request { id, method, .. }) => {
                // ch serves no requests; say so, so the plugin doesn't wait.
                let reply = Message::error(
                    id,
                    RpcError::new(
                        ch_plugin_protocol::code::METHOD_NOT_FOUND,
                        format!("ch doesn't serve `{method}`"),
                    ),
                );
                let _ = tx.try_send(reply.to_line());
            }
            Err(e) => tracing::debug!(plugin = name, "skipped a line that isn't JSON-RPC: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn responses_reach_their_calls_and_an_exit_fails_the_rest() {
        let (ch_side, mut plugin) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(ch_side);
        let notes = Arc::new(Mutex::new(Vec::new()));
        let conn = Conn::new(read, write, "t", {
            let notes = notes.clone();
            Box::new(move |m, p| notes.lock().unwrap().push((m.to_string(), p)))
        });
        let cancel = CancellationToken::new();
        let call = conn.call("x", json!({}), Duration::from_secs(5), &cancel);
        let answer = async {
            // Read the request, answer it with noise around.
            let mut buf = vec![0u8; 4096];
            let n = tokio::io::AsyncReadExt::read(&mut plugin, &mut buf)
                .await
                .unwrap();
            let req = Message::parse(&buf[..n - 1]).unwrap();
            let Message::Request { id, .. } = req else {
                panic!()
            };
            plugin.write_all(b"banner\n").await.unwrap();
            plugin
                .write_all(&Message::notification("log", json!({"message": "m"})).to_line())
                .await
                .unwrap();
            plugin
                .write_all(&Message::result(id, json!({"ok": 1})).to_line())
                .await
                .unwrap();
        };
        let (result, ()) = tokio::join!(call, answer);
        assert_eq!(result.unwrap(), json!({"ok": 1}));
        assert_eq!(notes.lock().unwrap()[0].0, "log");

        let pending = conn.call("y", json!({}), Duration::from_secs(5), &cancel);
        let exit = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(plugin);
        };
        let (result, ()) = tokio::join!(pending, exit);
        assert_eq!(result, Err(CallError::Closed));
        assert!(conn.is_closed());
    }

    #[test]
    fn ids_of_oversized_responses() {
        assert_eq!(
            oversized_id(br#"{"jsonrpc":"2.0","id":12,"result":{"content":"xx"#),
            Some(12)
        );
        assert_eq!(oversized_id(br#" { "id" : 3 , "result""#), Some(3));
        assert_eq!(oversized_id(br#"{"jsonrpc":"2.0","result":{"id":4,"#), None);
        assert_eq!(
            oversized_id(br#"{"jsonrpc":"2.0","id":12"#),
            None,
            "may be cut"
        );
    }

    #[tokio::test]
    async fn timeouts_send_a_cancel() {
        let (ch_side, mut plugin) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(ch_side);
        let conn = Conn::new(read, write, "t", Box::new(|_, _| {}));
        let cancel = CancellationToken::new();
        let r = conn
            .call("slow", json!({}), Duration::from_millis(50), &cancel)
            .await;
        assert_eq!(r, Err(CallError::Timeout(Duration::from_millis(50))));
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut buf = vec![0u8; 4096];
        let n = tokio::io::AsyncReadExt::read(&mut plugin, &mut buf)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(
            text.contains("\"slow\"") && text.contains("$/cancel"),
            "{text}"
        );
    }
}
