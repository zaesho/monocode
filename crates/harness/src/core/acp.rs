//! Port of src/integrations/harness/core/acp.ts: the ACP JSON-RPC client, a
//! thin wrapper over [`JsonRpcClient`] that hands adapters numeric request
//! ids and replies with the agent's original ids.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::json_rpc::{
    JsonRpcClient, JsonRpcClientOptions, JsonRpcHandlers, JsonRpcId, LineWriter, RpcErrorBody,
};

pub use super::json_rpc::JsonRpcMessage;

type NotificationHandler = Arc<dyn Fn(&str, Value) + Send + Sync>;
type RequestHandler = Arc<dyn Fn(i64, &str, Value) + Send + Sync>;

/// `AcpHandlers`.
#[derive(Clone, Default)]
pub struct AcpHandlers {
    pub on_notification: Option<NotificationHandler>,
    pub on_request: Option<RequestHandler>,
}

impl AcpHandlers {
    pub fn on_notification(
        mut self,
        handler: impl Fn(&str, Value) + Send + Sync + 'static,
    ) -> Self {
        self.on_notification = Some(Arc::new(handler));
        self
    }

    pub fn on_request(
        mut self,
        handler: impl Fn(i64, &str, Value) + Send + Sync + 'static,
    ) -> Self {
        self.on_request = Some(Arc::new(handler));
        self
    }
}

/// The largest id the remote host accepts for a UI approval
/// (`Number.MAX_SAFE_INTEGER`).
const MAX_UI_REQUEST_ID: i64 = (1 << 53) - 1;

/// Maps the numeric ids handed to adapters back to the agent's raw request
/// ids. Non-negative safe integer ids pass through. String ids and ids out of
/// that range get unused numbers counted down from [`MAX_UI_REQUEST_ID`].
struct RequestIds {
    raw: HashMap<i64, JsonRpcId>,
    next: i64,
}

impl Default for RequestIds {
    fn default() -> Self {
        Self {
            raw: HashMap::new(),
            next: MAX_UI_REQUEST_ID,
        }
    }
}

impl RequestIds {
    fn allocate(&mut self, id: JsonRpcId) -> i64 {
        let mut numeric = match id {
            JsonRpcId::Number(id) if (0..=MAX_UI_REQUEST_ID).contains(&id) => id,
            _ => self.take_next(),
        };
        while self.raw.contains_key(&numeric) {
            numeric = self.take_next();
        }
        self.raw.insert(numeric, id);
        numeric
    }

    fn take_next(&mut self) -> i64 {
        let next = self.next;
        self.next -= 1;
        next
    }

    /// The raw id for a reply. Each id answers once.
    fn take(&mut self, id: i64) -> JsonRpcId {
        self.raw.remove(&id).unwrap_or(JsonRpcId::Number(id))
    }
}

/// `AcpClient`. Clones share one connection.
#[derive(Clone)]
pub struct AcpClient {
    rpc: JsonRpcClient,
    request_ids: Arc<Mutex<RequestIds>>,
}

impl AcpClient {
    pub fn new(session_id: &str, writer: Arc<dyn LineWriter>, handlers: AcpHandlers) -> Self {
        Self::with_options(
            session_id,
            writer,
            handlers,
            JsonRpcClientOptions::default(),
        )
    }

    /// Like [`AcpClient::new`] with explicit client options. The label is
    /// always `acp` and `jsonrpc` is always sent.
    pub fn with_options(
        session_id: &str,
        writer: Arc<dyn LineWriter>,
        handlers: AcpHandlers,
        options: JsonRpcClientOptions,
    ) -> Self {
        let mut rpc_handlers = JsonRpcHandlers::default();
        if let Some(on_notification) = handlers.on_notification {
            rpc_handlers.on_notification = Some(on_notification);
        }
        let request_ids = Arc::new(Mutex::new(RequestIds::default()));
        if let Some(on_request) = handlers.on_request {
            let request_ids = request_ids.clone();
            rpc_handlers.on_request = Some(Arc::new(move |id: JsonRpcId, method: &str, params| {
                let numeric = request_ids.lock().allocate(id);
                on_request(numeric, method, params)
            }));
        }
        let rpc = JsonRpcClient::new(
            session_id,
            writer,
            rpc_handlers,
            JsonRpcClientOptions {
                include_jsonrpc: true,
                label: "acp".into(),
                ..options
            },
        );
        Self { rpc, request_ids }
    }

    /// The underlying client.
    pub fn rpc(&self) -> &JsonRpcClient {
        &self.rpc
    }

    pub fn push_line(&self, line: &str) {
        self.rpc.push_line(line);
    }

    pub fn close(&self, error: Option<&str>) {
        self.rpc.close(error);
        self.request_ids.lock().raw.clear();
    }

    pub fn reject_pending(&self, error: Option<&str>) {
        self.rpc.reject_pending(error);
    }

    pub fn is_closed(&self) -> bool {
        self.rpc.is_closed()
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_ms: i64,
    ) -> Result<T> {
        self.rpc.request(method, params, timeout_ms).await
    }

    pub async fn request_value(
        &self,
        method: &str,
        params: Option<Value>,
        timeout_ms: i64,
    ) -> Result<Value> {
        self.rpc.request_value(method, params, timeout_ms).await
    }

    pub async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        self.rpc.notify(method, params).await
    }

    /// Reply to the request the adapter knows as `id`, using the agent's
    /// original id.
    pub async fn respond(&self, id: i64, result: Value) -> Result<()> {
        let raw = self.request_ids.lock().take(id);
        self.rpc.respond(raw, result).await
    }

    pub async fn respond_error(&self, id: i64, error: RpcErrorBody) -> Result<()> {
        let raw = self.request_ids.lock().take(id);
        self.rpc.respond_error(raw, error).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::task::BoxFuture;

    struct Recorder(Mutex<Vec<String>>);

    impl LineWriter for Recorder {
        fn write_line(&self, _session_id: &str, line: String) -> BoxFuture<'static, Result<()>> {
            self.0.lock().push(line);
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn sends_jsonrpc_and_replies_with_the_original_request_id() {
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let ids = Arc::new(Mutex::new(Vec::new()));
        let seen = ids.clone();
        let client = AcpClient::new(
            "acp-1",
            recorder.clone(),
            AcpHandlers::default()
                .on_request(move |id, method, _| seen.lock().push((id, method.to_string()))),
        );
        client.push_line(r#"{"jsonrpc":"2.0","id":"12","method":"session/request_permission"}"#);
        client.push_line(r#"{"jsonrpc":"2.0","id":4,"method":"fs/read_text_file"}"#);
        assert_eq!(
            *ids.lock(),
            vec![
                (MAX_UI_REQUEST_ID, "session/request_permission".to_string()),
                (4, "fs/read_text_file".to_string())
            ]
        );
        smol::block_on(
            client.respond(MAX_UI_REQUEST_ID, serde_json::json!({ "outcome": "allow" })),
        )
        .unwrap();
        smol::block_on(client.notify(
            "session/cancel",
            Some(serde_json::json!({ "sessionId": "x" })),
        ))
        .unwrap();
        assert_eq!(
            *recorder.0.lock(),
            vec![
                r#"{"jsonrpc":"2.0","id":"12","result":{"outcome":"allow"}}"#.to_string(),
                r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"x"}}"#
                    .to_string(),
            ]
        );
    }

    #[test]
    fn maps_string_and_colliding_ids_to_unique_numbers_and_replies_with_raw_ids() {
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let ids = Arc::new(Mutex::new(Vec::new()));
        let seen = ids.clone();
        let client = AcpClient::new(
            "acp-ids",
            recorder.clone(),
            AcpHandlers::default().on_request(move |id, _, _| seen.lock().push(id)),
        );
        let incoming = [
            serde_json::json!("permission-abc"),
            serde_json::json!(MAX_UI_REQUEST_ID),
            serde_json::json!("12"),
            serde_json::json!(12),
            serde_json::json!(-1),
        ];
        for id in &incoming {
            client.push_line(
                &serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": "session/request_permission",
                    "params": {},
                })
                .to_string(),
            );
        }
        let numeric = ids.lock().clone();
        assert_eq!(
            numeric
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            incoming.len()
        );
        assert!(
            numeric
                .iter()
                .all(|id| (0..=MAX_UI_REQUEST_ID).contains(id))
        );
        for id in numeric {
            smol::block_on(client.respond(
                id,
                serde_json::json!({ "outcome": { "outcome": "cancelled" } }),
            ))
            .unwrap();
        }
        let sent: Vec<Value> = recorder
            .0
            .lock()
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["id"].clone())
            .collect();
        assert_eq!(sent, incoming.to_vec());
    }
}
