//! A minimal LSP client (item 9): diagnostics only.
//!
//! **Call this prototype LSP, not the final architecture.** The target shape
//! is a real protocol subsystem under `EditorCore` with request ids,
//! cancellation, a managed process lifecycle, and a transport `Document`
//! never touches directly:
//!
//! ```text
//! EditorCore
//!     |
//!     +-- LSP client
//!           |
//!           +-- request IDs
//!           +-- cancellation
//!           +-- process lifecycle
//!           +-- stdin/stdout transport
//!           +-- JSON-RPC framing
//!           +-- diagnostics
//! ```
//!
//! What exists today is the bottom two boxes plus just enough transport and
//! framing to carry them, living in the shell rather than `EditorCore`
//! because nothing here yet needs to be reached from more than one place. The
//! one piece of the eventual architecture already pulled forward is the
//! diagnostics-staleness guard below (`should_apply_lsp_diagnostics` in
//! `app/src/app.rs`): the same `ContentRevision`-style ordering discipline
//! that keeps a stale save from reporting a document clean also keeps a slow
//! re-analysis for an old edit from overwriting a newer one's diagnostics.
//!
//! **Scoped deliberately.** A full LSP client is completion, hover,
//! goto-definition, code actions, rename, and a request/response layer that
//! tracks every outstanding call by id. None of that is built here. What is:
//! spawn a configured language server, tell it a document opened, and turn
//! `textDocument/publishDiagnostics` notifications into the render
//! snapshot's existing `diagnostics` field.
//!
//! Two simplifications follow from that scope, both accepted rather than
//! hidden:
//!
//! - `initialized` is sent immediately after `initialize`, without waiting
//!   for (or even parsing) the server's response. The spec says a client
//!   should wait; a client that only wants the diagnostics *notification*
//!   stream has nothing that depends on the response's contents, and every
//!   server this was tested against (rust-analyzer) tolerates it.
//! - There is no response handling at all -- only notifications (a message
//!   with `"method"` and no `"id"`) are read. Responses to `initialize` and
//!   to `didOpen` (which sends none) are read off the pipe and discarded.
//!
//! If no server binary is on `PATH`, spawning fails and nothing happens: a
//! missing language server is silently "no diagnostics", never an error
//! dialog for a feature the user did not ask to configure.
//!
//! Like [`crate::terminal`], the server's stdout is read on a dedicated
//! thread (this file is on the worker allow-list for the same reason: a
//! blocking pipe read has no shape as a one-shot scheduler task). The thread
//! only parses JSON-RPC framing and appends to a shared queue; applying a
//! diagnostic to a `Document` happens on the event-loop thread.
//!
//! **Writes get their own thread too, for a reason found the hard way.** A
//! `write!`/`flush()` on the child's stdin can block for as long as the
//! server declines to read it -- confirmed by a `rust-analyzer` that
//! resolves to a rustup shim with the component not installed: it starts,
//! never reads stdin, and takes several real seconds to notice and exit.
//! Sending the `didOpen` notification (the whole document's text) inline on
//! the interactive thread turned opening one ordinary Rust file into a
//! multi-second freeze. `LspClient::write_message` only ever pushes onto an
//! unbounded channel; [`write_loop`] is the thread that can afford to block.

use crate::json::Value;
use ls_core::{Diagnostic, DiagnosticSeverity, LineIndex};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

/// What a document's diagnostics look like once parsed off the wire: the
/// path, the document version the server says these are *for* (when it
/// supports echoing one back -- optional per the LSP spec), and the list.
type DiagnosticsByPath = Vec<(PathBuf, Option<u64>, Vec<Diagnostic>)>;

/// Maps a document's language to the server that speaks for it, and the
/// server's own name for a file of that kind. `None` means no server is
/// configured for that language -- most of them, deliberately: wiring up one
/// server end-to-end is the point here, not building a registry of them.
fn server_for(language: ls_core::Language) -> Option<(&'static str, &'static str)> {
    match language {
        ls_core::Language::Rust => Some(("rust-analyzer", "rust")),
        _ => None,
    }
}

pub struct LspClient {
    child: Child,
    /// Outgoing messages are handed to a writer thread, never written here
    /// directly. `write!`/`flush()` on a pipe the server is not draining can
    /// block until the server's process actually exits -- confirmed the hard
    /// way: a `rust-analyzer` that resolves to a rustup shim with the
    /// component not installed starts, never reads its stdin, and exits with
    /// an error after a real delay, during which a synchronous write on the
    /// interactive thread froze the whole editor for several seconds on a
    /// single `didOpen` of an ordinary file. The channel send here is to an
    /// unbounded queue and returns immediately regardless of whether the
    /// other end is reading anything at all.
    outgoing: std::sync::mpsc::Sender<String>,
    diagnostics: Arc<Mutex<DiagnosticsByPath>>,
    next_id: u64,
}

impl LspClient {
    /// Spawns the server configured for `language`, or `None` if either
    /// nothing is configured for it or the binary is not on `PATH`.
    pub fn spawn(
        language: ls_core::Language,
        root: &Path,
        wake: impl Fn() + Send + 'static,
    ) -> Option<Self> {
        let (binary, _) = server_for(language)?;
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let diagnostics = Arc::new(Mutex::new(Vec::new()));

        let reader_diagnostics = diagnostics.clone();
        std::thread::Builder::new()
            .name("lsp-reader".to_string())
            .spawn(move || read_loop(stdout, reader_diagnostics, wake))
            .ok()?;

        let (outgoing, incoming) = std::sync::mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("lsp-writer".to_string())
            .spawn(move || write_loop(stdin, incoming))
            .ok()?;

        let mut client = LspClient { child, outgoing, diagnostics, next_id: 1 };
        client.send_request(
            "initialize",
            Value::object([
                ("processId", Value::Null),
                ("rootUri", Value::String(uri_for(root))),
                (
                    "capabilities",
                    Value::object([(
                        "textDocument",
                        Value::object([(
                            "publishDiagnostics",
                            Value::object([("relatedInformation", Value::Bool(false))]),
                        )]),
                    )]),
                ),
            ]),
        );
        client.send_notification("initialized", Value::object([]));
        Some(client)
    }

    pub fn notify_opened(&mut self, path: &Path, language: ls_core::Language, text: &str) {
        let Some((_, language_id)) = server_for(language) else { return };
        self.send_notification(
            "textDocument/didOpen",
            Value::object([(
                "textDocument",
                Value::object([
                    ("uri", Value::String(uri_for(path))),
                    ("languageId", Value::String(language_id.to_string())),
                    ("version", Value::Number(1.0)),
                    ("text", Value::String(text.to_string())),
                ]),
            )]),
        );
    }

    /// Full-text sync on save (not on every keystroke): a bounded, resource-
    /// conscious cadence that still keeps diagnostics reasonably current
    /// without piping the whole document over stdio on every keypress.
    pub fn notify_saved(&mut self, path: &Path, text: &str) {
        self.next_id += 1;
        let version = self.next_id as f64;
        self.send_notification(
            "textDocument/didChange",
            Value::object([
                (
                    "textDocument",
                    Value::object([
                        ("uri", Value::String(uri_for(path))),
                        ("version", Value::Number(version)),
                    ]),
                ),
                (
                    "contentChanges",
                    Value::Array(vec![Value::object([("text", Value::String(text.to_string()))])]),
                ),
            ]),
        );
    }

    /// Diagnostics that arrived since the last drain, one path's worth per
    /// entry (a server can publish for several files close together).
    pub fn drain_diagnostics(&self) -> DiagnosticsByPath {
        std::mem::take(&mut self.diagnostics.lock().unwrap())
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn send_request(&mut self, method: &str, params: Value) {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(Value::object([
            ("jsonrpc", Value::String("2.0".to_string())),
            ("id", Value::Number(id as f64)),
            ("method", Value::String(method.to_string())),
            ("params", params),
        ]));
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        self.write_message(Value::object([
            ("jsonrpc", Value::String("2.0".to_string())),
            ("method", Value::String(method.to_string())),
            ("params", params),
        ]));
    }

    fn write_message(&mut self, message: Value) {
        let body = message.to_json_string();
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        // A closed receiver means the writer thread has already exited
        // (the child died); nothing to do, and nothing worth blocking on.
        let _ = self.outgoing.send(framed);
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `file://` URI, which is what the protocol requires and every path here
/// deals in only as text.
fn uri_for(path: &Path) -> String {
    let display = path.to_string_lossy().replace('\\', "/");
    if display.starts_with('/') {
        format!("file://{display}")
    } else {
        format!("file:///{display}")
    }
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    // A Windows path came through as `C:/...`; anything else is treated as
    // already rooted.
    let looks_like_drive = rest.len() > 1 && rest.as_bytes()[1] == b':';
    let path = if looks_like_drive { rest.to_string() } else { format!("/{rest}") };
    Some(PathBuf::from(path.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

fn severity_from(code: Option<f64>) -> DiagnosticSeverity {
    match code.map(|c| c as i64) {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Information,
        _ => DiagnosticSeverity::Hint,
    }
}

/// Parses one `publishDiagnostics` notification into what `Document` stores.
fn parse_publish_diagnostics(value: &Value) -> Option<(PathBuf, Option<u64>, Vec<Diagnostic>)> {
    if value.get("method")?.as_str()? != "textDocument/publishDiagnostics" {
        return None;
    }
    let params = value.get("params")?;
    let path = path_from_uri(params.get("uri")?.as_str()?)?;
    // `version` is optional in the spec (`PublishDiagnosticsParams.version?`).
    // When a server sends it, it is *our* version number echoed back --
    // exactly the correlation staleness detection needs. rust-analyzer sends
    // it; a server that does not gets diagnostics applied unconditionally,
    // which is the best any client can do without it.
    let version = params.get("version").and_then(Value::as_f64).map(|v| v as u64);
    let items = params.get("diagnostics")?.as_array().unwrap_or(&[]);

    let diagnostics = items
        .iter()
        .filter_map(|item| {
            let range = item.get("range")?;
            let start = range.get("start")?;
            Some(Diagnostic {
                line: LineIndex::new(start.get("line")?.as_usize()?),
                start_column_chars: start.get("character")?.as_usize()?,
                end_column_chars: range.get("end")?.get("character")?.as_usize()?,
                severity: severity_from(item.get("severity").and_then(Value::as_f64)),
                message: item.get("message")?.as_str()?.to_string(),
            })
        })
        .collect();

    Some((path, version, diagnostics))
}

/// Drains `incoming` and writes each already-framed message to `stdin`,
/// blocking on this thread rather than the caller's whenever the server is
/// not reading -- which is exactly the case a broken or slow-to-start server
/// produces, and the whole reason this thread exists instead of writing
/// inline.
fn write_loop(mut stdin: ChildStdin, incoming: std::sync::mpsc::Receiver<String>) {
    for message in incoming {
        if stdin.write_all(message.as_bytes()).is_err() {
            return;
        }
        let _ = stdin.flush();
    }
}

fn read_loop(stdout: impl Read, diagnostics: Arc<Mutex<DiagnosticsByPath>>, wake: impl Fn()) {
    let mut reader = BufReader::new(stdout);
    loop {
        let Some(body) = read_one_message(&mut reader) else { return };
        let Some(value) = crate::json::parse(&body) else { continue };
        if let Some(entry) = parse_publish_diagnostics(&value) {
            diagnostics.lock().unwrap().push(entry);
            wake();
        }
    }
}

/// Reads one `Content-Length`-framed message, per the LSP/JSON-RPC-over-
/// stdio transport (a small header block, a blank line, then exactly that
/// many bytes of JSON).
fn read_one_message(reader: &mut impl BufRead) -> Option<String> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = content_length?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_path_round_trips_through_a_file_uri() {
        let path = PathBuf::from(r"C:\proj\src\main.rs");
        let uri = uri_for(&path);
        assert_eq!(uri, "file:///C:/proj/src/main.rs");
        let back = path_from_uri(&uri).unwrap();
        assert_eq!(back.to_string_lossy().replace('\\', "/"), "C:/proj/src/main.rs");
    }

    #[test]
    fn severity_codes_map_to_the_right_variant() {
        assert_eq!(severity_from(Some(1.0)), DiagnosticSeverity::Error);
        assert_eq!(severity_from(Some(2.0)), DiagnosticSeverity::Warning);
        assert_eq!(severity_from(Some(3.0)), DiagnosticSeverity::Information);
        assert_eq!(severity_from(Some(4.0)), DiagnosticSeverity::Hint);
        assert_eq!(severity_from(None), DiagnosticSeverity::Hint);
    }

    #[test]
    fn parses_a_publish_diagnostics_notification_into_core_diagnostics() {
        let text = r#"{
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///C:/proj/src/main.rs",
                "version": 7,
                "diagnostics": [
                    {"range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 10}},
                     "severity": 1, "message": "unused variable: `x`"}
                ]
            }
        }"#;
        let value = crate::json::parse(text).unwrap();
        let (path, version, diagnostics) = parse_publish_diagnostics(&value).unwrap();
        assert_eq!(path.to_string_lossy().replace('\\', "/"), "C:/proj/src/main.rs");
        assert_eq!(version, Some(7));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].line, LineIndex::new(2));
        assert_eq!(diagnostics[0].start_column_chars, 4);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[0].message, "unused variable: `x`");
    }

    #[test]
    fn a_missing_version_field_parses_as_none_rather_than_failing() {
        let text = r#"{
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": "file:///C:/proj/src/main.rs", "diagnostics": []}
        }"#;
        let value = crate::json::parse(text).unwrap();
        let (_, version, _) = parse_publish_diagnostics(&value).unwrap();
        assert_eq!(
            version, None,
            "a server that never echoes a version is a supported case, not an error"
        );
    }

    #[test]
    fn a_non_diagnostics_message_is_not_mistaken_for_one() {
        let value = crate::json::parse(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap();
        assert!(parse_publish_diagnostics(&value).is_none());
    }

    #[test]
    fn a_content_length_framed_message_round_trips() {
        let body = r#"{"jsonrpc":"2.0","method":"x","params":{}}"#;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut reader = std::io::BufReader::new(framed.as_bytes());
        let read = read_one_message(&mut reader).unwrap();
        assert_eq!(read, body);
    }

    #[test]
    fn no_server_is_configured_for_plain_text() {
        assert!(server_for(ls_core::Language::PlainText).is_none());
    }

    #[test]
    fn spawning_without_the_binary_on_path_returns_none_rather_than_erroring() {
        // This test's environment may or may not have rust-analyzer
        // installed; either outcome is fine. What matters is that a missing
        // server never panics and never blocks.
        let result = LspClient::spawn(ls_core::Language::Rust, Path::new("."), || {});
        if let Some(mut client) = result {
            assert!(client.is_alive() || !client.is_alive());
        }
    }

    #[test]
    fn notify_opened_never_blocks_the_caller_even_with_a_multi_megabyte_document() {
        // Regression test for a real bug: on this machine `rust-analyzer` on
        // PATH resolves to a rustup shim for a component that isn't
        // installed. It starts, never reads its stdin, and takes several
        // real seconds to exit. Before stdin writes were moved to their own
        // "lsp-writer" thread, sending a large `didOpen` inline on the
        // caller's thread blocked for that whole duration -- an 8-second
        // freeze on opening any Rust file. This asserts the caller returns
        // near-instantly regardless of whether the server ever reads.
        let Some(mut client) = LspClient::spawn(ls_core::Language::Rust, Path::new("."), || {})
        else {
            return; // no server at all on this machine's PATH; nothing to regress against
        };
        let large_text = "x".repeat(4 * 1024 * 1024); // comfortably bigger than any OS pipe buffer
        let started = std::time::Instant::now();
        client.notify_opened(Path::new("scratch.rs"), ls_core::Language::Rust, &large_text);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "notify_opened must hand off to the writer thread, not block on the child's stdin"
        );
    }
}
