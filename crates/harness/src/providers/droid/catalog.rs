//! Port of src/integrations/harness/providers/droid/droidCatalog.ts.
//!
//! Droid lists its models on `session/new` but reports reasoning levels only
//! for the selected one. The probe publishes the plain list first, then
//! walks the models on its throwaway session (a local switch, no inference)
//! to attach each model's own effort choices.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::future::Shared;
use parking_lot::Mutex;
use serde_json::{Value, json};

use monocode_core::harness::HarnessId;
use monocode_core::models::AgentModel;

use crate::core::acp::{AcpClient, AcpHandlers};
use crate::core::catalog::SharedCatalog;
use crate::core::child::{ChildHandlers, Children};
use crate::core::task::{self, BoxFuture, SharedSpawner};
use crate::providers::grok::shared::{initialize_params, spawn_method_not_found};

use super::protocol::{
    DROID_ACP_ARGS, DroidConfigOption, droid_config_options_from, droid_effort_config,
    droid_session_id, models_from_droid_session,
};

const PROBE_ID: &str = "monocode-droid-probe";
const DISCOVERY_TIMEOUT_MS: i64 = 60_000;
const REQUEST_TIMEOUT_MS: i64 = 20_000;
const CONFIG_SETTLE_MS: u64 = 250;

struct Inner {
    children: Children,
    spawner: SharedSpawner,
    catalog: SharedCatalog,
    inflight: Mutex<Option<Shared<BoxFuture<'static, ()>>>>,
}

/// The Droid catalog probe. Clones share one probe.
#[derive(Clone)]
pub struct DroidCatalog {
    inner: Arc<Inner>,
}

impl DroidCatalog {
    pub fn new(children: Children, spawner: SharedSpawner, catalog: SharedCatalog) -> Self {
        Self {
            inner: Arc::new(Inner {
                children,
                spawner,
                catalog,
                inflight: Mutex::new(None),
            }),
        }
    }

    /// `refreshDroidCatalog`. Concurrent calls share one probe.
    pub async fn refresh(&self) {
        let run = {
            let mut inflight = self.inner.inflight.lock();
            match inflight.as_ref() {
                Some(run) => run.clone(),
                None => {
                    let inner = self.inner.clone();
                    let run = async move {
                        let catalog = inner.catalog.clone();
                        let publish = move |models: Vec<AgentModel>, complete: bool| {
                            if !models.is_empty() {
                                catalog.set_harness_models_complete(
                                    HarnessId::Droid,
                                    models,
                                    complete,
                                );
                            }
                        };
                        if let Err(error) = probe_droid_models(
                            &inner.children,
                            &inner.spawner,
                            publish,
                            None,
                            DISCOVERY_TIMEOUT_MS,
                        )
                        .await
                        {
                            log::debug!("[monocode] droid catalog {error:#}");
                        }
                        inner.inflight.lock().take();
                    }
                    .boxed()
                    .shared();
                    *inflight = Some(run.clone());
                    run
                }
            }
        };
        run.await;
    }

    /// `discoverDroidModels`: the full catalog, including each model's effort
    /// choices.
    pub async fn discover(&self, working_directory: Option<&str>) -> Result<Vec<AgentModel>> {
        let latest = Arc::new(Mutex::new(Vec::new()));
        let sink = latest.clone();
        probe_droid_models(
            &self.inner.children,
            &self.inner.spawner,
            move |models, _complete| *sink.lock() = models,
            working_directory,
            DISCOVERY_TIMEOUT_MS,
        )
        .await?;
        Ok(latest.lock().clone())
    }
}

/// `probeDroidModels`.
async fn probe_droid_models(
    children: &Children,
    spawner: &SharedSpawner,
    publish: impl Fn(Vec<AgentModel>, bool),
    working_directory: Option<&str>,
    discovery_timeout_ms: i64,
) -> Result<()> {
    let path = children.resolve_droid_binary().await?.path;
    let cwd = match working_directory {
        Some(cwd) => cwd.to_string(),
        None => children.home_dir().await?,
    };
    let probe_id = format!("{PROBE_ID}-{}", uuid::Uuid::new_v4());
    let latest_config: Arc<Mutex<Option<Vec<DroidConfigOption>>>> = Arc::default();
    // The cell holds the client for request replies and is emptied in `stop`.
    let reply_client: Arc<Mutex<Option<AcpClient>>> = Arc::default();
    let handlers = AcpHandlers::default()
        .on_notification({
            let latest_config = latest_config.clone();
            move |method: &str, params: Value| {
                if method != "session/update" {
                    return;
                }
                if let Some(options) = droid_config_options_from(&params) {
                    *latest_config.lock() = Some(options);
                }
            }
        })
        .on_request({
            let reply_client = reply_client.clone();
            let spawner = spawner.clone();
            move |id: i64, method: &str, _params: Value| {
                if let Some(acp) = reply_client.lock().clone() {
                    spawn_method_not_found(&spawner, acp, id, method);
                }
            }
        });
    let acp = AcpClient::new(&probe_id, Arc::new(children.clone()), handlers);
    *reply_client.lock() = Some(acp.clone());

    let stop = || {
        let acp = acp.clone();
        let children = children.clone();
        let probe_id = probe_id.clone();
        let reply_client = reply_client.clone();
        async move {
            acp.close(None);
            reply_client.lock().take();
            children.unwatch_child(&probe_id);
            let _ = children.kill_child(&probe_id).await;
        }
    };

    children.watch_child_with(
        &probe_id,
        ChildHandlers {
            on_line: Box::new({
                let acp = acp.clone();
                move |line| acp.push_line(&line)
            }),
            on_exit: Box::new({
                let acp = acp.clone();
                move |_| acp.close(Some("Droid catalog probe exited"))
            }),
            on_stderr: None,
        },
    );

    let result = async {
        children
            .spawn_child(
                &probe_id,
                &path,
                DROID_ACP_ARGS.iter().map(|arg| arg.to_string()).collect(),
                &cwd,
                None,
                Some(HarnessId::Droid),
            )
            .await?;
        let probe = async {
            acp.request_value("initialize", Some(initialize_params("monocode")), REQUEST_TIMEOUT_MS)
                .await?;
            let created = acp
                .request_value(
                    "session/new",
                    Some(json!({ "cwd": cwd, "mcpServers": [] })),
                    REQUEST_TIMEOUT_MS,
                )
                .await?;
            let models = models_from_droid_session(&created, &HashMap::new());
            publish(models.clone(), false);
            let Some(session_id) = droid_session_id(&created) else {
                return Ok(());
            };
            if models.is_empty() {
                return Ok(());
            }

            let mut complete = true;
            let mut efforts: HashMap<String, DroidConfigOption> = HashMap::new();
            let initial = droid_effort_config(&droid_config_options_from(&created).unwrap_or_default()).cloned();
            for model in &models {
                let native = model.native_id.clone().unwrap_or_default();
                if native.is_empty() {
                    continue;
                }
                latest_config.lock().take();
                // Keep the model without effort choices rather than drop it.
                if let Ok(result) = acp
                    .request_value(
                        "session/set_config_option",
                        Some(json!({ "sessionId": session_id, "configId": "model", "value": native })),
                        REQUEST_TIMEOUT_MS,
                    )
                    .await
                {
                    let options = match droid_config_options_from(&result) {
                        Some(options) => Some(options),
                        None => settled_config(&latest_config).await,
                    };
                    if options.is_none() { complete = false; }
                    if let Some(effort) = options.as_deref().and_then(droid_effort_config) {
                        efforts.insert(native, effort.clone());
                    }
                    publish(models_from_droid_session(&created, &efforts), false);
                } else { complete = false; }
            }
            if efforts.is_empty()
                && let Some(initial) = initial
                && let Some(current) = models.first().and_then(|model| model.native_id.clone())
            {
                efforts.insert(current, initial);
            }
            publish(models_from_droid_session(&created, &efforts), complete);
            Ok(())
        };
        match task::timeout(task::ms(discovery_timeout_ms), probe).await {
            Some(result) => result,
            None => {
                stop().await;
                Err(anyhow!("Droid model discovery timed out"))
            }
        }
    }
    .await;
    stop().await;
    result
}

/// `settledConfig`: the `config_option_update` notification can trail the
/// empty response.
async fn settled_config(
    latest: &Mutex<Option<Vec<DroidConfigOption>>>,
) -> Option<Vec<DroidConfigOption>> {
    let deadline = Instant::now() + Duration::from_millis(CONFIG_SETTLE_MS);
    loop {
        let value = latest.lock().clone();
        if value.is_some() || Instant::now() >= deadline {
            return value;
        }
        task::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::grok::test_support::Peer;

    #[test]
    fn timeout_retains_enriched_models_and_allows_retry() {
        smol::block_on(async {
            let peer = Peer::new();
            let children = peer.ctx.children.clone();
            let spawner = peer.ctx.spawner.clone();
            let catalog = peer.ctx.catalog.clone();
            let observed = catalog.clone();
            let running = smol::spawn(async move {
                probe_droid_models(
                    &children,
                    &spawner,
                    move |models, complete| {
                        catalog.set_harness_models_complete(HarnessId::Droid, models, complete)
                    },
                    Some("/repo"),
                    150,
                )
                .await
            });
            let initialize = peer.next("initialize", |_| true).await;
            let id = peer
                .calls()
                .into_iter()
                .find_map(|call| match call {
                    crate::core::testing::Call::Spawn(request) => Some(request.session_id),
                    _ => None,
                })
                .unwrap();
            peer.reply(&id, &initialize["id"], json!({}));
            peer.answer(&id, "session/new", json!({ "sessionId": "probe", "models": { "currentModelId": "a", "availableModels": [ { "modelId": "a", "name": "A" }, { "modelId": "b", "name": "B" } ] } })).await;
            peer.answer(&id, "session/set_config_option", json!({ "configOptions": [ { "id": "reasoning_effort", "category": "thought_level", "currentValue": "high", "options": [ { "value": "high", "name": "High" }, { "value": "low", "name": "Low" } ] } ] })).await;
            assert!(running.await.is_err());
            assert!(!observed.has_live_catalog(HarnessId::Droid));
            assert!(
                observed
                    .read()
                    .find_model("droid:a")
                    .unwrap()
                    .settings
                    .is_some()
            );
            assert!(peer.calls().contains(&crate::core::testing::Call::Kill(id)));
            // A later successful probe completes the previously partial list.
            peer.clear();
            let children = peer.ctx.children.clone();
            let spawner = peer.ctx.spawner.clone();
            let catalog = observed.clone();
            let retry = smol::spawn(async move {
                probe_droid_models(
                    &children,
                    &spawner,
                    move |models, complete| {
                        catalog.set_harness_models_complete(HarnessId::Droid, models, complete)
                    },
                    Some("/repo"),
                    1000,
                )
                .await
            });
            let initialize = peer.next("initialize", |_| true).await;
            let id = peer
                .calls()
                .into_iter()
                .find_map(|call| match call {
                    crate::core::testing::Call::Spawn(request) => Some(request.session_id),
                    _ => None,
                })
                .unwrap();
            peer.reply(&id, &initialize["id"], json!({}));
            peer.answer(&id, "session/new", json!({ "sessionId": "probe-retry", "models": { "currentModelId": "a", "availableModels": [ { "modelId": "a", "name": "A" } ] } })).await;
            peer.answer(&id, "session/set_config_option", json!({ "configOptions": [ { "id": "reasoning_effort", "category": "thought_level", "currentValue": "high", "options": [ { "value": "high", "name": "High" }, { "value": "low", "name": "Low" } ] } ] })).await;
            retry.await.unwrap();
            assert!(observed.has_live_catalog(HarnessId::Droid));
        });
    }
}
