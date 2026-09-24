//! Receipt digest log.
//!
//! Forwards the digest of every signed receipt to an external append-only log,
//! for example a service that batches digests into Merkle roots and anchors them
//! on chain. The digest is the SHA-256 of the stored document, which is the
//! receipt's JCS form with `signature` included, so it equals what a verifier
//! computes from the served receipt. Only the receipt id and the digest leave
//! the gateway; the receipt stays in the store.
//!
//! Delivery is at least once and in order: digests queue in memory, a background
//! task posts them in batches, and a failed batch is retried with backoff before
//! anything behind it. The receiving log must ignore a digest it already has.
//! On shutdown, [`ReceiptLog::flush`] sends what is still queued.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::receipt_store::ReceiptStore;
use super::ReceiptOwner;
use crate::aci::receipt::SignedReceipt;

const DEFAULT_FLUSH_INTERVAL_MS: u64 = 200;
const DEFAULT_MAX_BATCH: usize = 500;
const DEFAULT_MAX_QUEUE: usize = 1_000_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// The gateway's optional `receipt_log` config section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptLogConfig {
    /// Endpoint that accepts `POST {"receipts":[{"receiptId","digest"}]}` and
    /// answers 2xx once the batch is durable.
    pub url: String,
    #[serde(default)]
    pub bearer_token: Option<String>,
    /// How often queued digests are sent. Defaults to 200 ms.
    #[serde(default)]
    pub flush_interval_ms: Option<u64>,
    /// Digests per request. Defaults to 500.
    #[serde(default)]
    pub max_batch: Option<usize>,
    /// Digests held while the log is unreachable; beyond this, new digests are
    /// dropped and counted. Defaults to 1,000,000.
    #[serde(default)]
    pub max_queue: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggedReceipt {
    pub receipt_id: String,
    /// `0x` + 64 lowercase hex.
    pub digest: String,
}

#[derive(Serialize)]
struct LogRequest<'a> {
    receipts: &'a [LoggedReceipt],
}

/// SHA-256 of the stored receipt document, as `0x`-prefixed hex.
pub fn receipt_digest(receipt: &SignedReceipt) -> String {
    format!("0x{}", hex::encode(Sha256::digest(&receipt.document)))
}

pub struct ReceiptLog {
    queue: Mutex<VecDeque<LoggedReceipt>>,
    /// Serializes senders, so the background task and a shutdown flush never
    /// post the same digests out of order.
    sending: tokio::sync::Mutex<()>,
    client: reqwest::Client,
    url: String,
    bearer_token: Option<String>,
    max_batch: usize,
    max_queue: usize,
    dropped: AtomicU64,
}

impl ReceiptLog {
    pub fn new(config: &ReceiptLogConfig) -> Result<Arc<Self>, String> {
        let url = config.url.trim().to_string();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("receipt_log.url must be an http(s) URL".to_string());
        }
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("receipt_log client: {e}"))?;
        Ok(Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            sending: tokio::sync::Mutex::new(()),
            client,
            url,
            bearer_token: config.bearer_token.clone(),
            max_batch: config.max_batch.unwrap_or(DEFAULT_MAX_BATCH).max(1),
            max_queue: config.max_queue.unwrap_or(DEFAULT_MAX_QUEUE).max(1),
            dropped: AtomicU64::new(0),
        }))
    }

    /// Queue one digest. Never blocks on the network.
    pub fn push(&self, entry: LoggedReceipt) {
        let mut queue = self.queue.lock().expect("receipt log queue poisoned");
        if queue.len() >= self.max_queue {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                receipt_id = %entry.receipt_id,
                dropped,
                "receipt log queue full; digest dropped"
            );
            return;
        }
        queue.push_back(entry);
    }

    pub fn pending(&self) -> usize {
        self.queue.lock().expect("receipt log queue poisoned").len()
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Send everything queued, batch by batch. Stops at the first failed batch,
    /// which goes back to the front of the queue.
    pub async fn flush(&self) -> Result<(), String> {
        let _sending = self.sending.lock().await;
        loop {
            let batch: Vec<LoggedReceipt> = {
                let mut queue = self.queue.lock().expect("receipt log queue poisoned");
                let n = queue.len().min(self.max_batch);
                queue.drain(..n).collect()
            };
            if batch.is_empty() {
                return Ok(());
            }
            if let Err(err) = self.send(&batch).await {
                let mut queue = self.queue.lock().expect("receipt log queue poisoned");
                for entry in batch.into_iter().rev() {
                    queue.push_front(entry);
                }
                return Err(err);
            }
        }
    }

    async fn send(&self, batch: &[LoggedReceipt]) -> Result<(), String> {
        let mut request = self
            .client
            .post(&self.url)
            .json(&LogRequest { receipts: batch });
        if let Some(token) = &self.bearer_token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("receipt log answered {}", response.status()));
        }
        Ok(())
    }

    /// Flush every `flush_interval_ms`, backing off (up to a minute) while the
    /// log is unreachable.
    pub fn spawn(self: &Arc<Self>, config: &ReceiptLogConfig) {
        let interval = Duration::from_millis(
            config
                .flush_interval_ms
                .unwrap_or(DEFAULT_FLUSH_INTERVAL_MS)
                .max(10),
        );
        let log = Arc::clone(self);
        tokio::spawn(async move {
            let mut delay = interval;
            loop {
                tokio::time::sleep(delay).await;
                match log.flush().await {
                    Ok(()) => delay = interval,
                    Err(err) => {
                        delay = (delay * 2).min(MAX_BACKOFF);
                        tracing::warn!(
                            error = %err,
                            pending = log.pending(),
                            retry_in_ms = delay.as_millis() as u64,
                            "receipt log delivery failed"
                        );
                    }
                }
            }
        });
    }
}

/// A [`ReceiptStore`] that also queues each stored receipt's digest on a
/// [`ReceiptLog`].
pub struct LoggingReceiptStore<S> {
    inner: S,
    log: Arc<ReceiptLog>,
}

impl<S> LoggingReceiptStore<S> {
    pub fn new(inner: S, log: Arc<ReceiptLog>) -> Self {
        Self { inner, log }
    }
}

impl<S: ReceiptStore> ReceiptStore for LoggingReceiptStore<S> {
    fn put(&self, receipt: SignedReceipt, owner: Option<ReceiptOwner>, now: u64, expires_at: u64) {
        self.log.push(LoggedReceipt {
            receipt_id: receipt.receipt_id.clone(),
            digest: receipt_digest(&receipt),
        });
        self.inner.put(receipt, owner, now, expires_at);
    }

    fn get_by_receipt_id(&self, receipt_id: &str, now: u64) -> Option<SignedReceipt> {
        self.inner.get_by_receipt_id(receipt_id, now)
    }

    fn get_by_chat_id(&self, chat_id: &str, now: u64) -> Option<SignedReceipt> {
        self.inner.get_by_chat_id(chat_id, now)
    }

    fn owner_of(&self, receipt_id: &str, now: u64) -> Option<ReceiptOwner> {
        self.inner.owner_of(receipt_id, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::service::InMemoryReceiptStore;
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use serde_json::Value;
    use std::sync::atomic::AtomicBool;

    #[derive(Clone, Default)]
    struct Sink {
        received: Arc<Mutex<Vec<Value>>>,
        auth: Arc<Mutex<Vec<Option<String>>>>,
        failing: Arc<AtomicBool>,
    }

    async fn ingest(
        State(sink): State<Sink>,
        headers: axum::http::HeaderMap,
        Json(body): Json<Value>,
    ) -> StatusCode {
        if sink.failing.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE;
        }
        sink.auth.lock().unwrap().push(
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        );
        for r in body["receipts"].as_array().unwrap() {
            sink.received.lock().unwrap().push(r.clone());
        }
        StatusCode::NO_CONTENT
    }

    async fn serve(sink: Sink) -> String {
        let app = Router::new()
            .route("/receipts", post(ingest))
            .with_state(sink);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/receipts")
    }

    fn receipt(id: &str, document: &str) -> SignedReceipt {
        SignedReceipt {
            receipt_id: id.to_string(),
            chat_id: None,
            document: document.as_bytes().to_vec(),
            key_id: "k".to_string(),
            signature_hex: "00".to_string(),
        }
    }

    fn config(url: String) -> ReceiptLogConfig {
        ReceiptLogConfig {
            url,
            bearer_token: Some("tok".to_string()),
            flush_interval_ms: None,
            max_batch: Some(2),
            max_queue: Some(3),
        }
    }

    #[test]
    fn digest_is_sha256_of_the_served_document() {
        // A production receipt as served (anchorer/test/fixtures/week0-receipt.json
        // in the envolvr repo): the anchorer's JCS digest of it is 0x358f0d33….
        let served = include_str!("../../../tests/fixtures/receipt-week0.json");
        assert_eq!(
            receipt_digest(&receipt("r", served.trim_end())),
            "0x358f0d33075793ed86c818d8371e9a39d873b4f361b6b426d967c45d1026db3f"
        );
    }

    #[tokio::test]
    async fn store_forwards_digests_in_order_and_still_stores() {
        let sink = Sink::default();
        let log = ReceiptLog::new(&config(serve(sink.clone()).await)).unwrap();
        let store = LoggingReceiptStore::new(InMemoryReceiptStore::default(), log.clone());
        for (id, doc) in [("a", "{\"a\":1}"), ("b", "{\"b\":2}"), ("c", "{\"c\":3}")] {
            store.put(receipt(id, doc), None, 1, 100);
        }
        assert!(store.get_by_receipt_id("b", 2).is_some());
        log.flush().await.unwrap();
        let got = sink.received.lock().unwrap().clone();
        let ids: Vec<&str> = got
            .iter()
            .map(|r| r["receiptId"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["a", "b", "c"],
            "max_batch 2: two requests, order kept"
        );
        assert_eq!(
            got[1]["digest"],
            format!("0x{}", hex::encode(Sha256::digest(b"{\"b\":2}")))
        );
        assert_eq!(sink.auth.lock().unwrap().len(), 2);
        assert!(sink
            .auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a.as_deref() == Some("Bearer tok")));
        assert_eq!(log.pending(), 0);
    }

    #[tokio::test]
    async fn failed_delivery_keeps_digests_queued_in_order() {
        let sink = Sink::default();
        sink.failing.store(true, Ordering::SeqCst);
        let log = ReceiptLog::new(&config(serve(sink.clone()).await)).unwrap();
        log.push(LoggedReceipt {
            receipt_id: "a".into(),
            digest: "0x01".into(),
        });
        log.push(LoggedReceipt {
            receipt_id: "b".into(),
            digest: "0x02".into(),
        });
        assert!(log.flush().await.is_err());
        assert_eq!(log.pending(), 2);
        log.push(LoggedReceipt {
            receipt_id: "c".into(),
            digest: "0x03".into(),
        });
        sink.failing.store(false, Ordering::SeqCst);
        log.flush().await.unwrap();
        let ids: Vec<String> = sink
            .received
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["receiptId"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn full_queue_drops_and_counts() {
        let log = ReceiptLog::new(&config("http://127.0.0.1:9/receipts".into())).unwrap();
        for i in 0..5 {
            log.push(LoggedReceipt {
                receipt_id: i.to_string(),
                digest: "0x00".into(),
            });
        }
        assert_eq!(log.pending(), 3);
        assert_eq!(log.dropped(), 2);
    }

    #[tokio::test]
    async fn background_task_delivers_without_an_explicit_flush() {
        let sink = Sink::default();
        let mut cfg = config(serve(sink.clone()).await);
        cfg.flush_interval_ms = Some(20);
        let log = ReceiptLog::new(&cfg).unwrap();
        log.spawn(&cfg);
        log.push(LoggedReceipt {
            receipt_id: "x".into(),
            digest: "0xab".into(),
        });
        for _ in 0..100 {
            if !sink.received.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(sink.received.lock().unwrap().len(), 1);
    }

    #[test]
    fn config_rejects_a_non_http_url() {
        let cfg = ReceiptLogConfig {
            url: "control:8787".into(),
            ..config(String::new())
        };
        assert!(ReceiptLog::new(&cfg).is_err());
    }
}
