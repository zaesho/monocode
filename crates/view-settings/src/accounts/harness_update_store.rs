//! Port of src/features/providers/model/harnessUpdateActions.ts: the version
//! checks and update runs that the launch notice and the CLI updates card in
//! Settings share, so an update started in one shows its progress in the
//! other.
//!
//! The TypeScript kept one copy per window, because each window had its own
//! JS runtime. The native windows share one app, so this is one store per
//! app, kept in a GPUI global.

use std::collections::HashMap;
use std::rc::Rc;

use futures::channel::oneshot;
use gpui::{App, AppContext as _, Context, Entity, Global, Task};
use monocode_core::HarnessId;

use super::harness_update_notice::{RowState, run_update};
use super::host::HarnessUpdateHost;
use super::model::{
    HarnessUpdate, HarnessVersionCheck, is_harness_version_behind, pending_harness_updates,
};

/// A successful update, kept so a check that overlapped it keeps the new
/// version.
struct FinishedUpdate {
    installed: String,
    latest: String,
    /// The value of `finished_updates` this update set.
    order: u64,
}

type CheckWaiter = oneshot::Sender<Vec<HarnessVersionCheck>>;

/// `HarnessUpdateSnapshot` and the actions that change it.
pub struct HarnessUpdateStore {
    host: Rc<dyn HarnessUpdateHost>,
    /// `None` until a check has finished.
    checks: Option<Vec<HarnessVersionCheck>>,
    checking: bool,
    runs: HashMap<HarnessId, RowState>,
    /// Callers waiting on the running check.
    waiters: Vec<CheckWaiter>,
    /// Callers that forced a check while another ran. The running check may
    /// have skipped the availability probe, so they get a fresh one after it.
    forced: Vec<CheckWaiter>,
    /// Counts successful updates. A check that started before an update
    /// finished may have run the old binary, so the version the update left
    /// wins for that harness.
    finished_updates: u64,
    last_updates: HashMap<HarnessId, FinishedUpdate>,
}

struct StoreGlobal(Entity<HarnessUpdateStore>);

impl Global for StoreGlobal {}

impl HarnessUpdateStore {
    /// The app's store. The first caller's host serves every later caller;
    /// every window's host reaches the same CLIs.
    pub fn global(host: Rc<dyn HarnessUpdateHost>, cx: &mut App) -> Entity<Self> {
        if let Some(store) = cx.try_global::<StoreGlobal>() {
            return store.0.clone();
        }
        let store = cx.new(|_| Self {
            host,
            checks: None,
            checking: false,
            runs: HashMap::new(),
            waiters: Vec::new(),
            forced: Vec::new(),
            finished_updates: 0,
            last_updates: HashMap::new(),
        });
        cx.set_global(StoreGlobal(store.clone()));
        store
    }

    /// The last finished check, or `None` before the first one.
    pub fn checks(&self) -> Option<&[HarnessVersionCheck]> {
        self.checks.as_deref()
    }

    pub fn checking(&self) -> bool {
        self.checking
    }

    pub fn run(&self, harness: HarnessId) -> RowState {
        self.runs.get(&harness).cloned().unwrap_or_default()
    }

    /// `checkInstalledHarnessVersions`: checks every installed harness that
    /// has a release feed. A call made while a check runs shares its result,
    /// unless it forces a fresh probe, which runs after it.
    pub fn check(&mut self, force: bool, cx: &mut Context<Self>) -> Task<Vec<HarnessVersionCheck>> {
        let (send, receive) = oneshot::channel();
        if !self.checking {
            self.waiters.push(send);
            self.start_check(force, cx);
        } else if force {
            self.forced.push(send);
        } else {
            self.waiters.push(send);
        }
        cx.spawn(async move |_, _| receive.await.unwrap_or_default())
    }

    fn start_check(&mut self, force: bool, cx: &mut Context<Self>) {
        self.checking = true;
        let started_after = self.finished_updates;
        let found = self.host.check_versions(force, cx);
        cx.spawn(async move |this, cx| {
            let found = found.await;
            this.update(cx, |this, cx| this.finish_check(found, started_after, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    fn finish_check(
        &mut self,
        found: Vec<HarnessVersionCheck>,
        started_after: u64,
        cx: &mut Context<Self>,
    ) {
        let checks: Vec<HarnessVersionCheck> = found
            .into_iter()
            .map(|check| match self.last_updates.get(&check.harness()) {
                Some(update) if update.order > started_after => with_update(&check, update),
                _ => check,
            })
            .collect();
        self.checks = Some(checks.clone());
        self.checking = false;
        for waiter in self.waiters.drain(..) {
            waiter.send(checks.clone()).ok();
        }
        if !self.forced.is_empty() {
            self.waiters = std::mem::take(&mut self.forced);
            self.start_check(true, cx);
        }
        cx.notify();
    }

    /// `runHarnessUpdate`. A second request for a harness already updating
    /// leaves the first run to finish.
    pub fn run_update(&mut self, update: HarnessUpdate, cx: &mut Context<Self>) {
        let harness = update.harness;
        if self.run(harness) == RowState::Updating {
            return;
        }
        self.runs.insert(harness, RowState::Updating);
        let run = run_update(self.host.clone(), update.clone(), cx);
        cx.spawn(async move |this, cx| {
            let result = run.await;
            this.update(cx, |this, cx| {
                if let RowState::Updated(version) = &result {
                    this.record_update(&update, version.clone());
                }
                this.runs.insert(harness, result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Updates these harnesses side by side.
    pub fn start(&mut self, targets: Vec<HarnessUpdate>, cx: &mut Context<Self>) {
        for update in targets {
            self.run_update(update, cx);
        }
    }

    /// `recordUpdate`. A check that finished while the update ran may have
    /// found a newer release than the one offered, so the harness can still
    /// be behind afterwards.
    fn record_update(&mut self, update: &HarnessUpdate, version: String) {
        self.finished_updates += 1;
        let finished = FinishedUpdate {
            installed: version,
            latest: update.latest.clone(),
            order: self.finished_updates,
        };
        if let Some(checks) = &mut self.checks {
            for check in checks.iter_mut() {
                if check.harness() == update.harness {
                    *check = with_update(check, &finished);
                }
            }
        }
        self.last_updates.insert(update.harness, finished);
    }

    /// `checkForHarnessUpdates`: the launch check, claimed once per app
    /// launch. It is unprompted, so a harness whose lookup failed is left out
    /// silently. Every installed harness is checked so Settings can list them
    /// all, but the notice offers only those shown in the model picker.
    pub fn launch_check(host: Rc<dyn HarnessUpdateHost>, cx: &mut App) -> Task<Vec<HarnessUpdate>> {
        if !host.claim_launch_check(cx) {
            return Task::ready(Vec::new());
        }
        let store = Self::global(host.clone(), cx);
        let check = store.update(cx, |store, cx| store.check(false, cx));
        cx.spawn(async move |cx| {
            let checks = check.await;
            cx.update(|cx| {
                pending_harness_updates(&checks)
                    .into_iter()
                    .filter(|update| host.is_picker_visible(update.harness, cx))
                    .collect()
            })
        })
    }
}

/// `withUpdate`. The installed version comes from the update. The release
/// comes from the check, since its feed lookup is the more recent one,
/// unless the check failed.
fn with_update(check: &HarnessVersionCheck, update: &FinishedUpdate) -> HarnessVersionCheck {
    let harness = check.harness();
    let latest = check
        .versions()
        .map_or_else(|| update.latest.clone(), |(_, latest)| latest.to_string());
    let installed = update.installed.clone();
    if is_harness_version_behind(harness, &installed, &latest) {
        HarnessVersionCheck::Behind {
            harness,
            installed,
            latest,
        }
    } else {
        HarnessVersionCheck::Current {
            harness,
            installed,
            latest,
        }
    }
}
