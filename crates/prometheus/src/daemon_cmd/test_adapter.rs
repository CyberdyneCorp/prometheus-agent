//! A file-backed messaging adapter for daemon tests (debug builds only,
//! enabled by `PMT_TEST_ADAPTER_DIR`): the platform `test` reads inbound
//! messages from `<dir>/<profile>/inbox/*.json` and appends what it sends
//! to `<dir>/<profile>/sent.jsonl`, so a test drives the real daemon's
//! gateway end to end; a file `<dir>/<profile>/down` makes every send
//! fail as a network error.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, ChatInfo, ChatType,
    Descriptor, Inbound, Interactive, Outgoing, SendError, Sent,
};
use serde_json::{Value, json};

/// The platform name.
pub(super) const PLATFORM: &str = "test";

/// The factory for `dir`.
pub(super) fn factory(dir: PathBuf) -> AdapterFactory {
    Arc::new(move |setup| {
        let root = dir.join(setup.scope.name().as_str());
        std::fs::create_dir_all(root.join("inbox"))
            .map_err(|e| pmt_core::error::Error::io(&pmt_core::error::codes::HOME_IO, &root, &e))?;
        Ok(Arc::new(FileAdapter {
            root,
            next: AtomicUsize::new(1),
            reader: Mutex::default(),
        }) as Arc<dyn Adapter>)
    })
}

struct FileAdapter {
    root: PathBuf,
    next: AtomicUsize,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn chat_type(text: &str) -> ChatType {
    ChatType::parse(text).unwrap_or(ChatType::Dm)
}

fn inbound(value: &Value) -> Inbound {
    let text = |key: &str| value[key].as_str().map(str::to_owned);
    let mut inbound = Inbound::text(
        chat_type(value["chat_type"].as_str().unwrap_or("dm")),
        &text("chat").unwrap_or_default(),
        &text("sender").unwrap_or_default(),
        &text("text").unwrap_or_default(),
    );
    inbound.message_id = text("id").unwrap_or_default();
    inbound.thread_id = text("thread");
    inbound.callback = text("callback");
    inbound.is_bot = value["is_bot"].as_bool().unwrap_or(false);
    inbound
}

/// Reads the inbox every 50 ms, oldest file name first.
async fn read(root: PathBuf, link: Arc<dyn AdapterLink>) {
    let inbox = root.join("inbox");
    loop {
        let mut files: Vec<PathBuf> = std::fs::read_dir(&inbox)
            .map(|dir| dir.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.retain(|p| p.extension().is_some_and(|e| e == "json"));
        files.sort();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let _ = std::fs::remove_file(&file);
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let admission = link.submit(inbound(&value)).await;
            append(&root, "admissions.jsonl", &json!({"admission": admission}));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn append(root: &Path, file: &str, value: &Value) {
    if let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(file))
    {
        let _ = writeln!(out, "{value}");
    }
}

impl Adapter for FileAdapter {
    fn describe(&self) -> Descriptor {
        Descriptor {
            supports_edit: true,
            interactive: Interactive::Native,
            stateless_send: true,
            ..Descriptor::new(PLATFORM, 4096)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        let task = tokio::spawn(read(self.root.clone(), link));
        if let Some(old) = self.reader.lock().expect("reader poisoned").replace(task) {
            old.abort();
        }
        Box::pin(async { Ok(()) })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(task) = self.reader.lock().expect("reader poisoned").take() {
            task.abort();
        }
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let outcome = if self.root.join("down").exists() {
            Err(SendError::Network("the test platform is down".into()))
        } else {
            let id = format!("m{}", self.next.fetch_add(1, Ordering::Relaxed));
            let buttons: Vec<&str> = message.buttons.iter().map(|b| b.data.as_str()).collect();
            append(
                &self.root,
                "sent.jsonl",
                &json!({"kind": "send", "chat": message.chat_id, "thread": message.thread_id,
                    "text": message.text, "buttons": buttons, "message_id": id,
                    "attachments": message.attachments}),
            );
            Ok(Sent {
                message_id: Some(id),
            })
        };
        Box::pin(async move { outcome })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            Ok(ChatInfo {
                name: chat_id.to_owned(),
                chat_type: ChatType::Dm,
            })
        })
    }

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        append(
            &self.root,
            "sent.jsonl",
            &json!({"kind": "edit", "message_id": message_id, "text": message.text}),
        );
        Box::pin(async { Ok(()) })
    }
}
