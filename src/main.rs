use anyhow::{Context, Result};
use lsp_types::*;
use pyfixy_lsp::FixtureIndex;
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

#[derive(Default)]
struct Server {
    root: Option<PathBuf>,
    documents: HashMap<PathBuf, String>,
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
                        self.root =
                            root_from_initialize(v.get("params").cloned().unwrap_or_default());
                        send(
                            &mut output,
                            json!({"jsonrpc":"2.0","id":id,"result": InitializeResult {
                                capabilities: ServerCapabilities {
                                    completion_provider: Some(CompletionOptions::default()),
                                    definition_provider: Some(OneOf::Left(true)),
                                    references_provider: Some(OneOf::Left(true)),
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
                    (Option::None, "textDocument/didOpen") => {
                        self.handle_did_open(v.get("params").cloned().unwrap_or_default())?;
                    }
                    (Option::None, "textDocument/didChange") => {
                        self.handle_did_change(v.get("params").cloned().unwrap_or_default())?;
                    }
                    (Option::None, "textDocument/didClose") => {
                        self.handle_did_close(v.get("params").cloned().unwrap_or_default())?;
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
        Ok(self
            .index()?
            .definition(&path, params.text_document_position_params.position)?
            .map(GotoDefinitionResponse::Scalar))
    }

    fn handle_did_open(&mut self, params: Value) -> Result<()> {
        let params: DidOpenTextDocumentParams = serde_json::from_value(params)?;
        if let Ok(path) = params.text_document.uri.to_file_path() {
            self.documents.insert(path, params.text_document.text);
        }
        Ok(())
    }

    fn handle_did_change(&mut self, params: Value) -> Result<()> {
        let params: DidChangeTextDocumentParams = serde_json::from_value(params)?;
        if let Ok(path) = params.text_document.uri.to_file_path() {
            if let Some(change) = params.content_changes.into_iter().last() {
                self.documents.insert(path, change.text);
            }
        }
        Ok(())
    }

    fn handle_did_close(&mut self, params: Value) -> Result<()> {
        let params: DidCloseTextDocumentParams = serde_json::from_value(params)?;
        if let Ok(path) = params.text_document.uri.to_file_path() {
            self.documents.remove(&path);
        }
        Ok(())
    }

    fn handle_references(&self, params: Value) -> Result<Vec<Location>> {
        let params: ReferenceParams = serde_json::from_value(params)?;
        let path = params
            .text_document_position
            .text_document
            .uri
            .to_file_path()
            .unwrap();
        self.index()?
            .references(&path, params.text_document_position.position)
    }
}

fn root_from_initialize(params: Value) -> Option<PathBuf> {
    params
        .get("rootUri")
        .and_then(Value::as_str)
        .and_then(|uri| Url::parse(uri).ok())
        .and_then(|uri| uri.to_file_path().ok())
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
