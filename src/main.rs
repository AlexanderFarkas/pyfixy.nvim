use anyhow::{Context, Result};
use lsp_types::*;
use pyfixy_lsp::{DiagnosticConfig, FixtureIndex};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{self, BufRead, Write},
    path::PathBuf,
};
use url::Url;

fn main() -> Result<()> {
    Server::default().run()
}

struct Server {
    root: Option<PathBuf>,
    documents: HashMap<PathBuf, String>,
    diagnostics: DiagnosticConfig,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            root: None,
            documents: HashMap::new(),
            diagnostics: DiagnosticConfig::default(),
        }
    }
}

impl Server {
    fn run(&mut self) -> Result<()> {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut output = io::stdout();
        loop {
            let Some(msg) = read_message(&mut input)? else {
                break;
            };
            let v: Value = serde_json::from_str(&msg)?;
            if let Some(method) = v.get("method").and_then(Value::as_str) {
                let id = v.get("id").cloned();
                match (id, method) {
                    (Some(id), "initialize") => {
                        let init_params = v.get("params").cloned().unwrap_or_default();
                        self.root = root_from_initialize(init_params.clone());
                        self.diagnostics = diagnostics_from_initialize(init_params);
                        send(
                            &mut output,
                            json!({"jsonrpc":"2.0","id":id,"result": InitializeResult {
                                capabilities: ServerCapabilities {
                                    completion_provider: Some(CompletionOptions::default()),
                                    definition_provider: Some(OneOf::Left(true)),
                                    references_provider: Some(OneOf::Left(true)),
                                    code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                                    text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
                                    ..Default::default()
                                },
                                server_info: Some(ServerInfo { name: "pyfixy-lsp".into(), version: Some(env!("CARGO_PKG_VERSION").into()) }),
                            }}),
                        )?;
                    }
                    (Some(id), "textDocument/completion") => {
                        let result =
                            self.handle_completion(v.get("params").cloned().unwrap_or_default())?;
                        send(
                            &mut output,
                            json!({"jsonrpc":"2.0","id":id,"result":result}),
                        )?;
                    }
                    (Some(id), "textDocument/definition") => {
                        let result =
                            self.handle_definition(v.get("params").cloned().unwrap_or_default())?;
                        send(
                            &mut output,
                            json!({"jsonrpc":"2.0","id":id,"result":result}),
                        )?;
                    }
                    (Some(id), "textDocument/references") => {
                        let result =
                            self.handle_references(v.get("params").cloned().unwrap_or_default())?;
                        send(
                            &mut output,
                            json!({"jsonrpc":"2.0","id":id,"result":result}),
                        )?;
                    }
                    (Some(id), "textDocument/codeAction") => {
                        let result =
                            self.handle_code_action(v.get("params").cloned().unwrap_or_default())?;
                        send(
                            &mut output,
                            json!({"jsonrpc":"2.0","id":id,"result":result}),
                        )?;
                    }
                    (Option::None, "textDocument/didOpen") => {
                        if let Some((uri, diagnostics)) =
                            self.handle_did_open(v.get("params").cloned().unwrap_or_default())?
                        {
                            send(
                                &mut output,
                                json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":diagnostics}}),
                            )?;
                        }
                    }
                    (Option::None, "textDocument/didChange") => {
                        if let Some((uri, diagnostics)) =
                            self.handle_did_change(v.get("params").cloned().unwrap_or_default())?
                        {
                            send(
                                &mut output,
                                json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":diagnostics}}),
                            )?;
                        }
                    }
                    (Option::None, "textDocument/didClose") => {
                        if let Some(uri) =
                            self.handle_did_close(v.get("params").cloned().unwrap_or_default())?
                        {
                            send(
                                &mut output,
                                json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[]}}),
                            )?;
                        }
                    }
                    (Some(id), "shutdown") => {
                        send(&mut output, json!({"jsonrpc":"2.0","id":id,"result":null}))?
                    }
                    (Some(id), _) => {
                        send(&mut output, json!({"jsonrpc":"2.0","id":id,"result":null}))?
                    }
                    (Option::None, "exit") => break,
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn index(&self) -> Result<FixtureIndex> {
        FixtureIndex::build(self.root.as_ref().context("not initialized")?)
    }

    fn handle_completion(&self, params: Value) -> Result<Vec<CompletionItem>> {
        let params: CompletionParams = serde_json::from_value(params)?;
        let path = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        let index = self.index()?;
        let path = normalize_path(&path);
        if let Some(text) = self.documents.get(&path) {
            index.completions_for_text(&path, text, params.text_document_position.position)
        } else {
            index.completions(&path, params.text_document_position.position)
        }
    }

    fn handle_definition(&self, params: Value) -> Result<Option<GotoDefinitionResponse>> {
        let params: GotoDefinitionParams = serde_json::from_value(params)?;
        let path = params
            .text_document_position_params
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        let path = normalize_path(&path);
        Ok(self
            .index()?
            .definition(&path, params.text_document_position_params.position)?
            .map(GotoDefinitionResponse::Scalar))
    }

    fn handle_did_open(&mut self, params: Value) -> Result<Option<(Url, Vec<Diagnostic>)>> {
        let params: DidOpenTextDocumentParams = serde_json::from_value(params)?;
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            let path = normalize_path(&path);
            let text = params.text_document.text;
            let diagnostics = self
                .index()?
                .diagnostics_for_text(&path, &text, self.diagnostics)?;
            self.documents.insert(path, text);
            return Ok(Some((uri, diagnostics)));
        }
        Ok(None)
    }

    fn handle_did_change(&mut self, params: Value) -> Result<Option<(Url, Vec<Diagnostic>)>> {
        let params: DidChangeTextDocumentParams = serde_json::from_value(params)?;
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            let path = normalize_path(&path);
            if let Some(change) = params.content_changes.into_iter().last() {
                let diagnostics =
                    self.index()?
                        .diagnostics_for_text(&path, &change.text, self.diagnostics)?;
                self.documents.insert(path, change.text);
                return Ok(Some((uri, diagnostics)));
            }
        }
        Ok(None)
    }

    fn handle_did_close(&mut self, params: Value) -> Result<Option<Url>> {
        let params: DidCloseTextDocumentParams = serde_json::from_value(params)?;
        if let Ok(path) = params.text_document.uri.to_file_path() {
            self.documents.remove(&normalize_path(&path));
            return Ok(Some(params.text_document.uri));
        }
        Ok(None)
    }

    fn handle_references(&self, params: Value) -> Result<Vec<Location>> {
        let params: ReferenceParams = serde_json::from_value(params)?;
        let path = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        let path = normalize_path(&path);
        self.index()?
            .references(&path, params.text_document_position.position)
    }

    fn handle_code_action(&self, params: Value) -> Result<Vec<CodeAction>> {
        let params: CodeActionParams = serde_json::from_value(params)?;
        let uri = params.text_document.uri;
        let path = normalize_path(&uri.to_file_path().unwrap());
        let text = self
            .documents
            .get(&path)
            .cloned()
            .unwrap_or_else(|| std::fs::read_to_string(&path).unwrap_or_default());
        self.index()?
            .code_actions_for_text_range(&path, &text, uri, Some(params.range))
    }
}

fn diagnostics_from_initialize(params: Value) -> DiagnosticConfig {
    let mut config = DiagnosticConfig::default();
    let diagnostics = params
        .get("initializationOptions")
        .and_then(|v| v.get("diagnostics"));
    if let Some(v) = diagnostics
        .and_then(|d| d.get("missing_annotation"))
        .and_then(Value::as_str)
    {
        config.missing_annotation = severity_from_str(v).unwrap_or(config.missing_annotation);
    }
    if let Some(v) = diagnostics
        .and_then(|d| d.get("mismatched_annotation"))
        .and_then(Value::as_str)
    {
        config.mismatched_annotation = severity_from_str(v).unwrap_or(config.mismatched_annotation);
    }
    config
}

fn severity_from_str(value: &str) -> Option<DiagnosticSeverity> {
    match value.to_ascii_lowercase().as_str() {
        "error" => Some(DiagnosticSeverity::ERROR),
        "warning" | "warn" => Some(DiagnosticSeverity::WARNING),
        "information" | "info" => Some(DiagnosticSeverity::INFORMATION),
        "hint" => Some(DiagnosticSeverity::HINT),
        _ => None,
    }
}

fn normalize_path(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn root_from_initialize(params: Value) -> Option<PathBuf> {
    params
        .get("rootUri")
        .and_then(Value::as_str)
        .and_then(|uri| Url::parse(uri).ok())
        .and_then(|uri| uri.to_file_path().ok())
        .map(|path| normalize_path(&path))
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<String>> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            len = Some(value.parse::<usize>()?);
        }
    }
    let Some(len) = len else {
        return Ok(None);
    };
    let mut buf = vec![0; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8(buf)?))
}

fn send<W: Write>(writer: &mut W, value: Value) -> Result<()> {
    let body = serde_json::to_string(&value)?;
    write!(writer, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    writer.flush()?;
    Ok(())
}
