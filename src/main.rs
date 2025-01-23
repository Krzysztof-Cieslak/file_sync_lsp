use std::fs::OpenOptions;
use std::io;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use lsp_textdocument::FullTextDocument;
use lsp_types::Uri;
use notify::{Error, Event, Watcher};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time;
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

#[derive(Debug, Deserialize, Serialize)]
struct BranchChangedNotificationParams {
    branch: String,
}

impl BranchChangedNotificationParams {
    fn new(branch: impl Into<String>) -> Self {
        BranchChangedNotificationParams {
            branch: branch.into(),
        }
    }
}

enum BranchChangedNotification {}

impl Notification for BranchChangedNotification {
    type Params = BranchChangedNotificationParams;

    const METHOD: &'static str = "$/branchChanged";
}

struct Backend {
    client: Client,
    documents: Arc<DashMap<Uri, FullTextDocument>>,
    root_uri: RwLock<Option<Uri>>,
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
    let path = uri.path();

    let mut file_read = OpenOptions::new()
        .write(true)
        .create(true)
        .read(true)
        .open(path.as_str())?;

    let mut buf = String::new();
    file_read.read_to_string(&mut buf)?;
    if content != buf {
        let mut file_write = OpenOptions::new()
            .write(true)
            .create(true)
            .read(true)
            .truncate(true)
            .open(path.as_str())?;
        println!("  - Writing changes to disk");
        file_write.write_all(content.as_bytes())?;
        file_write.flush()?;
    } else {
        println!("  - No changes to sync");
    }

    Ok(())
}

async fn sync(documents: &Arc<DashMap<Uri, FullTextDocument>>) {
    for doc in documents.iter() {
        let d = doc.value();
        let len = d.content_len();
        println!("Syncing: {}, length: {}", doc.key().as_str(), len);
        let result = sync_to_disk(doc.value(), doc.key());
        if let Err(e) = result {
            eprintln!("  Error syncing file: {}", e);
        }
    }
}

pub struct GitBranchWatcher {
    client: Arc<RwLock<Option<Client>>>,
    head_path: &'static Path,
}

impl GitBranchWatcher {
    pub fn new(client: Arc<RwLock<Option<Client>>>) -> Self {
        Self {
            client,
            head_path: Path::new(".git/HEAD"),
        }
    }

    pub fn start(&self) -> notify::Result<()> {
        let watcher_client = self.client.clone();
        let head_path = self.head_path;
        
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, Error>| {
            let cl = watcher_client.clone();
            match res {
                Ok(event) => {
                    tokio::spawn(async move {
                        if let Err(e) = Self::handle_head_change(cl, head_path).await {
                            eprintln!("Failed to handle HEAD change: {}", e);
                        }
                    });
                    println!("watch event: {:?}", event.kind);
                }
                Err(e) => eprintln!("watch error: {}", e),
            }
        })?;

        watcher.watch(self.head_path, notify::RecursiveMode::NonRecursive)?;
        Ok(())
    }

    async fn handle_head_change(
        client: Arc<RwLock<Option<Client>>>,
        head_path: &Path,
    ) -> io::Result<()> {
        let mut file = OpenOptions::new().read(true).open(head_path)?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;

        if let Some(branch) = buf.strip_prefix("ref: ") {
            let branch = branch.trim();
            let client_guard = client.read().await;
            if let Some(client) = client_guard.as_ref() {
                client
                    .send_notification::<BranchChangedNotification>(
                        BranchChangedNotificationParams::new(branch),
                    )
                    .await;
            }
        }

        Ok(())
    }
}

pub struct ToDiskSyncer {
    interval: time::Interval,
    documents: Arc<DashMap<Uri, FullTextDocument>>,
}

impl ToDiskSyncer {
    pub fn new(documents: Arc<DashMap<Uri, FullTextDocument>>) -> Self {
        let interval = time::interval(Duration::from_secs(5));
        Self {
            interval,
            documents,
        }
    }

    pub async fn run(&mut self) {
        loop {
            self.interval.tick().await;
            sync(&self.documents).await;
        }
    }
    
    pub fn start(documents: Arc<DashMap<Uri, FullTextDocument>>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut syncer = Self::new(documents);
            syncer.run().await;
        })
    }
}

#[tokio::main]
async fn main() {
    let documents = Arc::new(DashMap::new());
    let client = Arc::new(RwLock::new(None::<Client>));

    //Initialize and start the file syncer
    ToDiskSyncer::start(documents.clone());

    // Initialize and start the git watcher
    let watcher = GitBranchWatcher::new(client.clone());
    if let Err(e) = watcher.start() {
        eprintln!("Failed to start git watcher: {}", e);
    }
    
    let listener = TcpListener::bind("127.0.0.1:1910").await.unwrap();
    let active_task = Arc::new(RwLock::new(None::<JoinHandle<()>>));

    loop {
        println!("Waiting for next connection...");
        let (stream, _) = listener.accept().await.unwrap();
        println!("Accepted connection from {:?}", stream.peer_addr().unwrap());

        // Cancel and discard the old task if any
        if let Some(handle) = active_task.write().await.take() {
            handle.abort();
            println!("Canceled previous connection handle");
        }

        let documents = documents.clone();
        let cl = client.clone();
        // Spawn a new task for the current connection
        let handle = tokio::spawn(async move {
            let (service, socket) = LspService::new(|client| Backend {
                client,
                documents,
                root_uri: RwLock::new(None),
            });
            *cl.write().await = Some(service.inner().client.clone());
            let (read, write) = tokio::io::split(stream);
            Server::new(read, write, socket).serve(service).await;
        });
        *active_task.write().await = Some(handle);
    }
}
