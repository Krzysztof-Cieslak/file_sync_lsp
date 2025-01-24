use std::fs::OpenOptions;
use std::io;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use lsp_textdocument::FullTextDocument;
use lsp_types::Uri;
use notify::{Error, Event, Watcher};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::sync::watch;
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
    uri_changed_tx: watch::Sender<Option<Uri>>,
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

            println!("[LSP] Initialized: {:?}", uri);
            let ur = uri.clone();
            let mut u = self.root_uri.write().await;
            *u = uri;
            let _ = self.uri_changed_tx.send(ur);
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
        println!("[LSP] Opened: {}", text_document.uri.as_str());
        
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
        println!("[LSP] Changed: {}", uri.as_str());
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
        println!("[LSP] Closed: {}", params.text_document.uri.as_str());
        self.client
            .log_message(MessageType::INFO, "file closed!")
            .await;
    }
}

pub struct GitBranchWatcher {
    client: Arc<RwLock<Option<Client>>>,
    watch_uri_rx: watch::Receiver<Option<Uri>>,
}

impl GitBranchWatcher {
    pub fn new(client: Arc<RwLock<Option<Client>>>, watch_uri_rx: watch::Receiver<Option<Uri>>) -> Self {
        Self {
            client,
            watch_uri_rx,
        }
    }

    pub fn start(&self) -> notify::Result<()> {
        println!("[GitBranchWatcher] Starting git branch watcher");
        let watch_uri_rx = self.watch_uri_rx.clone();
        let client = self.client.clone();

        let (watch_res_tx, mut watch_res_rx) = watch::channel(None::<(PathBuf, Result<Event, Error>)>);
        
        let cl = client.clone();
        tokio::spawn(async move {
            loop {
                println!("[GitBranchWatcher] Waiting for next head change...");
                if watch_res_rx.changed().await.is_err() {
                    println!("[GitBranchWatcher] Error watching for head changes");
                    break;
                }
                let path_to_process = match watch_res_rx.borrow().as_ref() {
                    Some((pb, Ok(_))) => {Some(pb.clone())},
                    _ => None,
                };

                if let Some(path) = path_to_process {
                    if let Err(e) = Self::handle_head_change(cl.clone(), &path).await {
                        println!("[GitBranchWatcher] Error handling head change: {}", e);
                    }
                }
            }
        });

        tokio::spawn(async move {
            let mut rx = watch_uri_rx;
            let mut current_watcher: Option<notify::RecommendedWatcher> = None;
            
            loop {
                let res_tx = watch_res_tx.clone();
                if rx.changed().await.is_err() {
                    println!("[GitBranchWatcher] Error watching for URI changes");
                    break;
                }
    
                // Drop any existing watcher
                println!("[GitBranchWatcher] Dropping existing watcher");
                drop(current_watcher.take());
    
                // Create a new watcher
                if let Some(new_uri) = rx.borrow().as_ref().cloned() {
                    let p = PathBuf::new().join(new_uri.path().as_str()).join(".git/HEAD");
                    println!("[GitBranchWatcher] Watching {:?}", p);
                    let pb = p.clone();
                    match notify::recommended_watcher(move |res: Result<Event, Error>| {
                        println!("[GitBranchWatcher] Received watch event");
                        match res {
                            Ok(event) => {
                                if event.kind.is_modify() {
                                    let _ = res_tx.send(Some((p.clone(), Ok(event))));
                                }
                            }
                            Err(e) => println!("[GitBranchWatcher] watch error: {}", e),
                        }
                    }) {
                        Ok(mut watcher) => {
                            println!("[GitBranchWatcher] Created watcher");
                            let _ = watcher.watch(pb.as_path(), notify::RecursiveMode::NonRecursive);
                            current_watcher = Some(watcher);
                        }
                        Err(e) => println!("[GitBranchWatcher] Could not create watcher: {}", e),
                    }
                }
            }
        });
    
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
                println!("[GitBranchWatcher] Notified client of branch change: {}", branch);
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
            ToDiskSyncer::sync(&self.documents).await;
        }
    }
    
    pub fn start(documents: Arc<DashMap<Uri, FullTextDocument>>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut syncer = Self::new(documents);
            syncer.run().await;
        })
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
            println!("[ToDiskSyncer]  - Writing changes to disk");
            file_write.write_all(content.as_bytes())?;
            file_write.flush()?;
        } else {
            println!("[ToDiskSyncer]  - No changes to sync");
        }
    
        Ok(())
    }
    
    async fn sync(documents: &Arc<DashMap<Uri, FullTextDocument>>) {
        for doc in documents.iter() {
            let d = doc.value();
            let len = d.content_len();
            println!("[ToDiskSyncer] Syncing: {}, length: {}", doc.key().as_str(), len);
            let result = ToDiskSyncer::sync_to_disk(doc.value(), doc.key());
            if let Err(e) = result {
                println!("[ToDiskSyncer]  - Error syncing file: {}", e);
            }
        }
    }
}

#[derive(Debug)]
struct TcpServerConfig {
    port: u16,
}

struct TcpLspServer {
    config: TcpServerConfig,
    documents: Arc<DashMap<Uri, FullTextDocument>>,
    client: Arc<RwLock<Option<Client>>>,
    active_task: Arc<RwLock<Option<JoinHandle<()>>>>,
    watch_uri_tx: watch::Sender<Option<Uri>>,
}

impl TcpLspServer {
    fn new(
        config: TcpServerConfig,
        documents: Arc<DashMap<Uri, FullTextDocument>>,
        client: Arc<RwLock<Option<Client>>>,
        watch_uri_tx: watch::Sender<Option<Uri>>,
    ) -> Self {
        Self {
            config,
            documents,
            client,
            active_task: Arc::new(RwLock::new(None)),
            watch_uri_tx,
        }
    }

    pub async fn start(&self) -> io::Result<()> {
        // Always binds to localhost, using only the configured port
        let address = format!("127.0.0.1:{}", self.config.port);
        let listener = TcpListener::bind(&address).await?;
        
        loop {
            println!("[TcpServer] Waiting for next connection...");
            let (stream, _) = listener.accept().await?;
            println!("[TcpServer] Accepted connection from {:?}", stream.peer_addr()?);

            // Cancel previous connection handle if active
            if let Some(handle) = self.active_task.write().await.take() {
                handle.abort();
                println!("[TcpServer] Canceled previous connection handle");
            }

            let documents = self.documents.clone();
            let cl = self.client.clone();
            let watch_uri_tx = self.watch_uri_tx.clone();
            let new_task = tokio::spawn(async move {
                let (service, socket) = LspService::new(|c| Backend {
                    client: c,
                    documents,
                    root_uri: RwLock::new(None),
                    uri_changed_tx: watch_uri_tx,
                });
                *cl.write().await = Some(service.inner().client.clone());
                let (read, write) = tokio::io::split(stream);
                Server::new(read, write, socket).serve(service).await;
            });
            println!("[TcpServer] Spawned new connection handle");

            *self.active_task.write().await = Some(new_task);
        }
    }
}

#[tokio::main]
async fn main() {
    let documents = Arc::new(DashMap::new());
    let client = Arc::new(RwLock::new(None::<Client>));
    let (watch_uri_tx, watch_uri_rx) = watch::channel(None::<Uri>);

    //Initialize and start the file syncer
    ToDiskSyncer::start(documents.clone());

    // Initialize and start the git watcher
    let watcher = GitBranchWatcher::new(client.clone(), watch_uri_rx);
    if let Err(e) = watcher.start() {
        println!("[Main] Failed to start git watcher: {}", e);
    }
    
    // Configure only the port; address is always localhost
    let server_config = TcpServerConfig { port: 1910 };
    let lsp_server = TcpLspServer::new(server_config, documents, client, watch_uri_tx);

    if let Err(e) = lsp_server.start().await {
        println!("[Main] Server failed: {}", e);
    }
}
