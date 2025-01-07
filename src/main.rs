use std::fs::OpenOptions;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use lsp_textdocument::FullTextDocument;
use lsp_types::Uri;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::{ task, time};
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types::notification::Notification;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug, Deserialize, Serialize)]
struct FileUpdatedNotificationParams {
    uri: Uri,
    message: String,
}

impl FileUpdatedNotificationParams {
    fn new(title: impl Into<Uri>, message: impl Into<String>) -> Self {
        FileUpdatedNotificationParams {
            uri: title.into(),
            message: message.into(),
        }
    }
}

enum FileUpdatedNotification {}

impl Notification for FileUpdatedNotification {
    type Params = FileUpdatedNotificationParams;

    const METHOD: &'static str = "$/fileUpdated";
}

struct Backend {
    client: Client,
    documents: Arc<DashMap<Uri, FullTextDocument>>,
    root_uri: RwLock<Option<Uri>>,
}

fn hash<T: Hash>(t: &T) -> u64 {
    let mut s = DefaultHasher::new();
    t.hash(&mut s);
    s.finish()
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> LspResult<InitializeResult> {
        {
            let uri = params
                .workspace_folders
                .unwrap_or_default()
                .into_iter()
                .next()
                .map(|f| f.uri)
                .or(params.root_uri);

            println!("Initialized: {:?}", uri);
            let mut u = self.root_uri.write().await;
            *u = uri;
        }
        //TODO: start file watcher
        // client.send_notification::<FileUpdatedNotification>(FileUpdatedNotificationParams::new(
        //     "file:///foo",
        //     "bar",
        // ));

        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                ..ServerCapabilities::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "initialized!")
            .await;
    }

    async fn shutdown(&self) -> LspResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let text_document = params.text_document;
        let document = FullTextDocument::new(
            text_document.language_id,
            text_document.version,
            text_document.text,
        );
        println!("Opened: {}", text_document.uri.as_str());
        
        self.documents.insert(text_document.uri.clone(), document);

        // let doc = self.documents.get(&text_document.uri).unwrap();
        // let sync_res = self.sync(&doc, &text_document.uri);
        // if let Err(e) = sync_res {
        //     self.client
        //         .log_message(MessageType::ERROR, format!("Error syncing file: {}", e))
        //         .await;
        // } else {
        //     self.client
        //         .log_message(MessageType::INFO, "file synced!")
        //         .await;
        // }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        println!("Changed: {}", uri.as_str());
        let doc = self.documents.get_mut(&uri);
        if let Some(mut doc) = doc {
            let doc = doc.value_mut();
            doc.update(&params.content_changes, params.text_document.version);
        } else {
            self.client
                .log_message(MessageType::ERROR, "file not opened!")
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        println!("Closed: {}", params.text_document.uri.as_str());
        self.client
            .log_message(MessageType::INFO, "file closed!")
            .await;
    }
}


fn sync_to_disk(doc: &FullTextDocument, uri: &Uri) -> io::Result<()> {
    let content = doc.get_content(None);
    let doc_hash = hash(&content);
    let path = uri.path();

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .read(true)
        .open(path.as_str())
        .unwrap();

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let file_hash = hash(&buf);
    if doc_hash != file_hash {
        file.write_all(content.as_bytes())?;
    }

    Ok(())
}

async fn sync(documents: &Arc<DashMap<Uri, FullTextDocument>>) {
    for doc in documents.iter() {
        println!("Syncing: {}", doc.key().as_str());
        let result = sync_to_disk(doc.value(), doc.key());
        if let Err(e) = result {
            eprintln!("  Error syncing file: {}", e);
        }
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args();
    let stream = match args.nth(1).as_deref() {
        None => {
            // If no argument is supplied (args is just the program name), then
            // we presume that the client has opened the TCP port and is waiting
            // for us to connect. This is the connection pattern used by clients
            // built with vscode-langaugeclient.
            TcpStream::connect("127.0.0.1:1910").await.unwrap()
        }
        Some("--listen") => {
            // If the `--listen` argument is supplied, then the roles are
            // reversed: we need to start a server and wait for the client to
            // connect.
            let listener = TcpListener::bind("127.0.0.1:1910").await.unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            stream
        }
        Some(arg) => panic!(
            "Unrecognized argument: {}. Use --listen to listen for connections.",
            arg
        ),
    };
    let (read, write) = tokio::io::split(stream);
    let documents = Arc::new(DashMap::new());
    let sync_docs = documents.clone();
    
    
    task::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(5));

        loop {
            interval.tick().await;
            sync(&sync_docs).await;
        }
    });
    
    let (service, socket) = LspService::new(|client| Backend {
        client,
        documents: documents.clone(),
        root_uri: RwLock::new(None),
    });


    Server::new(read, write, socket).serve(service).await;
}
