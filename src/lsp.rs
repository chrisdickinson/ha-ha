//! A minimal LSP client: just enough protocol to ask a language server for
//! shape, spoken over a child process's stdio.
//!
//! We use three methods — `documentSymbol`, `hover`, and (later) `references`.
//! That is small enough that a typed protocol crate would cost more than it
//! pays, and keeping the surface visibly small is the point: if this file has
//! to grow a lot to add a language, the thin-adapter bet is in trouble.

use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::Result;

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    stderr: Arc<Mutex<String>>,
    next_id: i64,
    debug: bool,
}

impl Client {
    /// Spawn `argv` with piped stdio and start pumping its output.
    pub fn spawn(argv: &[String], root: &Path) -> Result<Client> {
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start `{}`: {e}", argv[0]))?;

        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");

        let (tx, rx) = channel();
        thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            while let Ok(Some(frame)) = read_frame(&mut r) {
                match serde_json::from_slice::<Value>(&frame) {
                    Ok(v) => {
                        if tx.send(v).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Keep stderr around so a crash reports something better than EOF.
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            let mut r = BufReader::new(child_stderr);
            let mut line = String::new();
            while matches!(r.read_line(&mut line), Ok(n) if n > 0) {
                if let Ok(mut buf) = sink.lock() {
                    buf.push_str(&line);
                }
                line.clear();
            }
        });

        Ok(Client {
            child,
            stdin,
            rx,
            stderr,
            next_id: 1,
            debug: std::env::var_os("HAHA_LSP_DEBUG").is_some(),
        })
    }

    // ---- protocol ----

    /// Handshake. Returns the server's advertised capabilities.
    pub fn initialize(&mut self, root: &Path, timeout: Duration) -> Result<Value> {
        let uri = path_to_uri(root);
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into());
        let caps = self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "workspaceFolders": [{ "uri": uri, "name": name }],
                "capabilities": {
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "textDocument": {
                        "synchronization": { "didSave": false },
                        // Hierarchical results give us `children`, which is where
                        // container→member structure (and thus ids) comes from.
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "hover": { "contentFormat": ["markdown", "plaintext"] },
                        "references": {},
                    },
                    "window": { "workDoneProgress": true },
                    // rust-analyzer's readiness signal; ignored by other servers.
                    "experimental": { "serverStatusNotification": true },
                },
            }),
            timeout,
        )?;
        self.notify("initialized", json!({}))?;
        Ok(caps)
    }

    /// Block until the server says it is done indexing, or `timeout` elapses.
    ///
    /// Not an error to time out: `hover` retries on its own, and servers that
    /// send no readiness signal at all are answering already.
    pub fn wait_ready(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let _ = self.pump(deadline, |m| {
            let params = m.get("params")?;
            match m.get("method")?.as_str()? {
                "experimental/serverStatus" => params.get("quiescent")?.as_bool()?.then_some(()),
                "$/progress" => {
                    let token = params.get("token")?.as_str().unwrap_or("").to_lowercase();
                    let kind = params.get("value")?.get("kind")?.as_str()?;
                    (kind == "end" && token.contains("index")).then_some(())
                }
                _ => None,
            }
        });
    }

    pub fn did_open(&mut self, path: &Path, language_id: &str) -> Result<()> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(path),
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )
    }

    pub fn document_symbol(&mut self, path: &Path, timeout: Duration) -> Result<Value> {
        self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": path_to_uri(path) } }),
            timeout,
        )
    }

    pub fn hover(&mut self, path: &Path, line: u32, character: u32, timeout: Duration) -> Result<Value> {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "position": { "line": line, "character": character },
            }),
            timeout,
        )
    }

    // ---- plumbing ----

    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;

        let deadline = Instant::now() + timeout;
        let reply = self.pump(deadline, |m| {
            // A response: has our id and no method.
            (m.get("method").is_none() && m.get("id").and_then(Value::as_i64) == Some(id))
                .then(|| m.clone())
        })?;

        if let Some(err) = reply.get("error") {
            return Err(format!("{method} failed: {err}").into());
        }
        Ok(reply.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    /// Read messages until `want` accepts one, answering anything the server
    /// asks us along the way.
    ///
    /// Answering matters: rust-analyzer blocks on `workspace/configuration`, so
    /// a client that only listens for its own responses deadlocks here.
    fn pump<T>(&mut self, deadline: Instant, mut want: impl FnMut(&Value) -> Option<T>) -> Result<T> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err("timed out waiting for the language server".into());
            }
            let msg = match self.rx.recv_timeout(left) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => {
                    return Err("timed out waiting for the language server".into());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("language server exited{}", self.stderr_tail()).into());
                }
            };
            if self.debug {
                eprintln!("<-- {msg}");
            }
            if let Some(found) = want(&msg) {
                return Ok(found);
            }
            // A server→client *request* (has both method and id) needs a reply.
            if let (Some(method), Some(id)) = (msg.get("method"), msg.get("id")) {
                let result = default_reply(method.as_str().unwrap_or(""), msg.get("params"));
                self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))?;
            }
        }
    }

    fn send(&mut self, msg: &Value) -> Result<()> {
        let body = serde_json::to_vec(msg)?;
        if self.debug {
            eprintln!("--> {msg}");
        }
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn stderr_tail(&self) -> String {
        let buf = match self.stderr.lock() {
            Ok(b) => b,
            Err(_) => return String::new(),
        };
        let tail: Vec<&str> = buf.lines().rev().take(5).collect();
        if tail.is_empty() {
            String::new()
        } else {
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!(":\n{}", tail.join("\n"))
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.send(&json!({ "jsonrpc": "2.0", "id": 0, "method": "shutdown" }));
        let _ = self.send(&json!({ "jsonrpc": "2.0", "method": "exit" }));
        // Don't wait on a server that ignores us.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// What to answer for a server→client request we don't otherwise handle.
fn default_reply(method: &str, params: Option<&Value>) -> Value {
    match method {
        // Must be one entry per requested section, or servers mis-index the reply.
        "workspace/configuration" => {
            let n = params
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array(vec![Value::Null; n])
        }
        _ => Value::Null,
    }
}

/// Read one `Content-Length`-framed message. `Ok(None)` on clean EOF.
fn read_frame(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let Some(len) = len else { return Ok(None) };
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// `file://` URI with percent-encoding. Servers reject raw spaces in paths.
pub fn path_to_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_framed_messages_back_to_back() {
        let raw = b"Content-Length: 2\r\n\r\n{}Content-Type: x\r\nContent-Length: 4\r\n\r\n[1,2]";
        let mut r = BufReader::new(&raw[..]);
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"{}");
        // Header order varies between servers; length must still be found.
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"[1,2");
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn encodes_spaces_but_not_separators() {
        assert_eq!(
            path_to_uri(Path::new("/a b/c.rs")),
            "file:///a%20b/c.rs"
        );
    }

    #[test]
    fn configuration_reply_is_one_null_per_item() {
        let params = json!({ "items": [{ "section": "rust-analyzer" }, { "section": "x" }] });
        assert_eq!(
            default_reply("workspace/configuration", Some(&params)),
            json!([null, null])
        );
        assert_eq!(default_reply("client/registerCapability", None), Value::Null);
    }
}
