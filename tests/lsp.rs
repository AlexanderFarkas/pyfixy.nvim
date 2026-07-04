use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use tempfile::TempDir;

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Lsp {
    fn start() -> Self {
        let exe = env!("CARGO_BIN_EXE_pyfixy-lsp");
        let mut child = Command::new(exe)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let msg = self.read();
            if msg.get("id").and_then(Value::as_u64) == Some(id) {
                return msg;
            }
        }
    }

    fn send(&mut self, value: Value) {
        let body = serde_json::to_string(&value).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut len = None;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                len = Some(value.parse::<usize>().unwrap());
            }
        }
        let mut buf = vec![0; len.unwrap()];
        self.stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.request("shutdown", json!(null));
        self.send(json!({ "jsonrpc": "2.0", "method": "exit" }));
        let _ = self.child.wait();
    }
}

fn write(path: &std::path::Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn initialize_advertises_completion_and_definition() {
    let tmp = TempDir::new().unwrap();
    let mut lsp = Lsp::start();
    let response = lsp.request(
        "initialize",
        json!({ "rootUri": url::Url::from_directory_path(tmp.path()).unwrap() }),
    );
    let caps = &response["result"]["capabilities"];
    assert!(caps.get("completionProvider").is_some());
    assert_eq!(caps["definitionProvider"], true);
    assert_eq!(caps["referencesProvider"], true);
}

#[test]
fn completion_uses_unsaved_did_open_text() {
    let tmp = TempDir::new().unwrap();
    let conftest = tmp.path().join("tests/conftest.py");
    let test = tmp.path().join("tests/test_app.py");
    write(
        &conftest,
        "import pytest\n@pytest.fixture\ndef db(): pass\n",
    );
    write(&test, "def test_thing(): pass\n");

    let mut lsp = Lsp::start();
    lsp.request(
        "initialize",
        json!({ "rootUri": url::Url::from_directory_path(tmp.path()).unwrap() }),
    );
    lsp.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": url::Url::from_file_path(&test).unwrap(),
                "languageId": "python",
                "version": 1,
                "text": "def test_thing(existing, ): pass\n"
            }
        }
    }));

    let completion = lsp.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": url::Url::from_file_path(&test).unwrap() },
            "position": { "line": 0, "character": 25 }
        }),
    );
    assert!(completion["result"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["label"] == "db"));
}

#[test]
fn completion_and_definition_work_over_stdio() {
    let tmp = TempDir::new().unwrap();
    let conftest = tmp.path().join("tests/conftest.py");
    let test = tmp.path().join("tests/test_app.py");
    write(
        &conftest,
        "import pytest\n@pytest.fixture\ndef db(): pass\n",
    );
    write(&test, "def test_thing(db): pass\n");

    let mut lsp = Lsp::start();
    lsp.request(
        "initialize",
        json!({ "rootUri": url::Url::from_directory_path(tmp.path()).unwrap() }),
    );

    let completion = lsp.request(
        "textDocument/completion",
        json!({
            "textDocument": { "uri": url::Url::from_file_path(&test).unwrap() },
            "position": { "line": 0, "character": 15 }
        }),
    );
    assert!(completion["result"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["label"] == "db"));

    let definition = lsp.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": url::Url::from_file_path(&test).unwrap() },
            "position": { "line": 0, "character": 15 }
        }),
    );
    assert_eq!(
        definition["result"]["uri"],
        url::Url::from_file_path(&conftest).unwrap().to_string()
    );

    let references = lsp.request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": url::Url::from_file_path(&conftest).unwrap() },
            "position": { "line": 2, "character": 5 },
            "context": { "includeDeclaration": false }
        }),
    );
    let test_uri = url::Url::from_file_path(test).unwrap().to_string();
    assert!(references["result"]
        .as_array()
        .unwrap()
        .iter()
        .any(|loc| loc["uri"] == test_uri));
}
