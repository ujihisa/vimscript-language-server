// Copyright 2019 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::lexer::Lexer;
use crate::lexer::TokenPosition;
use crate::parser::Parser;
use crate::rename::rename;
use crate::server::LspSender;
use crate::server::Message;
use crate::server::Read;
use crate::server::Request;
use crate::server::Server;
use crate::server::Write;
use lsp_types::Diagnostic;
use lsp_types::DiagnosticSeverity;
use lsp_types::DidChangeTextDocumentParams;
use lsp_types::DidOpenTextDocumentParams;
use lsp_types::DocumentHighlight;
use lsp_types::DocumentHighlightParams;
use lsp_types::Position;
use lsp_types::PublishDiagnosticsParams;
use lsp_types::RenameParams;
use lsp_types::WorkspaceEdit;

use lsp_types::Range;
use lsp_types::Url;
use serde_json::json;
use std::collections::HashMap;

/// Runs the main loop of the LSP server.
///
/// This method finishes when `exit` notification is received.
pub fn run<R: Read, W: Write + Send + 'static>(server: Server<R, W>) {
    let mut state = State {
        files: HashMap::new(),
        sender: server.sender(),
    };
    for msg in server {
        state.handle_message(msg);
    }
}

struct State {
    files: HashMap<String, String>,
    sender: LspSender,
}

fn token_position_to_range(position: &TokenPosition) -> Range {
    Range {
        start: Position {
            line: position.start.line as u64,
            character: position.start.character as u64,
        },
        end: Position {
            line: position.end.line as u64,
            character: position.end.character as u64,
        },
    }
}

impl State {
    fn handle_message(&mut self, msg: Message) {
        match msg {
            Message::Request(req) => match req.method.as_ref() {
                "initialize" => {
                    req.response_handle.respond(Ok(json!({"capabilities": {
                        "renameProvider": true,
                        "documentHighlightProvider": true,
                    }})));
                }
                "textDocument/rename" => {
                    self.handle_rename(req);
                }
                "textDocument/documentHighlight" => {
                    self.handle_document_highlight(req);
                }
                method => {
                    eprintln!("Unrecognized request: {}", method);
                }
            },
            Message::Notification(notification) => match notification.method.as_ref() {
                "initialized" => {}
                "textDocument/didOpen" => {
                    let params: DidOpenTextDocumentParams =
                        serde_json::from_value(notification.params.clone()).unwrap();
                    self.handle_did_open(params);
                }
                "textDocument/didChange" => {
                    let params: DidChangeTextDocumentParams =
                        serde_json::from_value(notification.params.clone()).unwrap();
                    self.handle_did_change(params);
                }
                method => {
                    eprintln!("Unrecognized notification: {}", method);
                }
            },
        }
    }

    fn handle_did_open(&mut self, params: DidOpenTextDocumentParams) {
        self.files.insert(
            params.text_document.uri.as_str().to_string(),
            params.text_document.text.to_string(),
        );

        publish_diagnostics(
            &params.text_document.text,
            params.text_document.uri,
            &self.sender,
        );
    }

    fn handle_did_change(&mut self, params: DidChangeTextDocumentParams) {
        // TODO: Add support for partial content changes
        if params.content_changes.len() != 1 {
            panic!("unsupported not one content changes");
        }
        if !params.content_changes[0].range.is_none() {
            panic!("unsupported partial content change");
        }
        self.files.insert(
            params.text_document.uri.as_str().to_string(),
            params.content_changes[0].text.to_string(),
        );
        publish_diagnostics(
            &params.content_changes[0].text,
            params.text_document.uri,
            &self.sender,
        );
    }

    fn handle_rename(&self, req: Request) {
        // TODO: This doesn't work yet, it is still WIP!
        let params: RenameParams = serde_json::from_value(req.params.clone()).unwrap();
        let content = self
            .files
            .get(params.text_document_position.text_document.uri.as_str())
            .unwrap();
        let edits = rename(
            content,
            params.text_document_position.position,
            &params.new_name,
        )
        .unwrap();
        let mut changes = HashMap::new();
        changes.insert(params.text_document_position.text_document.uri, edits);
        req.response_handle
            .respond(Ok(serde_json::to_value(WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
            })
            .unwrap()))
    }

    fn handle_document_highlight(&self, req: Request) {
        // TODO: This doesn't work yet, it is still WIP!
        let params: DocumentHighlightParams = serde_json::from_value(req.params.clone()).unwrap();
        let start = params.text_document_position_params.position;
        let mut end = params.text_document_position_params.position;
        end.character += 2;
        req.response_handle
            .respond(Ok(serde_json::to_value(vec![DocumentHighlight {
                kind: None,
                range: Range {
                    start: start,
                    end: end,
                },
            }])
            .unwrap()));
    }
}

fn publish_diagnostics(text: &str, uri: Url, sender: &LspSender) {
    let mut parser = Parser::new(Lexer::new(text));
    parser.parse();
    let mut diagnostics_params = PublishDiagnosticsParams {
        uri: uri,
        diagnostics: Vec::new(),
        version: None,
    };
    for error in parser.errors {
        diagnostics_params.diagnostics.push(Diagnostic {
            range: token_position_to_range(&error.position),
            message: error.message,
            code: None,
            related_information: None,
            severity: Some(DiagnosticSeverity::Error),
            source: None,
            tags: None,
        });
    }
    sender.send_notification(
        "textDocument/publishDiagnostics",
        serde_json::to_value(diagnostics_params).unwrap(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::mpsc::channel;
    use std::sync::mpsc::Receiver;
    use std::sync::mpsc::Sender;

    struct FakeReader {
        receiver: Receiver<String>,
    }

    impl FakeReader {
        fn new() -> (Sender<String>, FakeReader) {
            let (sender, receiver) = channel();
            return (sender, FakeReader { receiver: receiver });
        }
    }

    impl Read for FakeReader {
        fn read_packet(&mut self) -> Result<String, io::Error> {
            self.receiver
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::UnexpectedEof, "EOF encountered"))
        }
    }
    struct FakeWriter {
        sender: Sender<String>,
    }

    impl FakeWriter {
        fn new() -> (Receiver<String>, FakeWriter) {
            let (sender, receiver) = channel();
            return (receiver, FakeWriter { sender: sender });
        }
    }

    impl Write for FakeWriter {
        fn write_packet(&self, packet: String) -> Result<(), io::Error> {
            self.sender.send(packet).unwrap();
            Ok(())
        }
    }

    struct Client {
        sender: Sender<String>,
        receiver: Receiver<String>,
    }

    impl Client {
        fn recv(&self) -> Result<serde_json::Value, ()> {
            Ok(self.receiver.recv().unwrap().parse().unwrap())
        }
        fn send(&self, req: serde_json::Value) -> Result<(), ()> {
            self.sender.send(req.to_string()).unwrap();
            Ok(())
        }
    }

    fn create_client_and_server() -> (Client, Server<FakeReader, FakeWriter>) {
        let (writer_ch, writer) = FakeWriter::new();
        let (reader_ch, reader) = FakeReader::new();
        let client = Client {
            sender: reader_ch,
            receiver: writer_ch,
        };
        let server = Server::new(reader, writer);
        return (client, server);
    }

    #[test]
    fn responds_to_initialize() {
        let (client, server) = create_client_and_server();
        let t = std::thread::spawn(move || {
            run(server);
        });

        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": serde_json::Value::Null,
                    "rootUri": serde_json::Value::Null,
                    "capabilities": {
                    },
                },
            }))
            .unwrap();
        client.recv().unwrap();
        client
            .send(json!({
                "jsonrpc": "2.0",
                "method": "exit",
            }))
            .unwrap();

        t.join().unwrap();
    }
}
