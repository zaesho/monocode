//! Port of HarnessUpdateNotice.test.ts, harnessUpdateActions.test.ts, and
//! the CLI updates test in SettingsView.test.ts.

use gpui::{AppContext as _, TestAppContext};

use super::*;
use crate::accounts::harness_update_notice::{HarnessUpdateNotice, RowState};
use crate::accounts::harness_update_store::HarnessUpdateStore;
use crate::accounts::harness_updates_card::{HarnessUpdatesCard, row_description};
use crate::accounts::host::{HarnessUpdateHost, OnHarnessUpdated};

type Listener = Rc<dyn Fn(HarnessId, &mut App)>;

/// A fake host: Claude is installed at 2.1.284 and its newest release is
/// 2.1.285. Each updater installs the newest release, and other windows can
/// announce updates. A check reads the installed versions when it starts.
pub(super) struct FakeUpdates {
    pub installed: RefCell<HashMap<HarnessId, String>>,
    pub latest: RefCell<HashMap<HarnessId, String>>,
    pub refreshed: RefCell<Vec<HarnessId>>,
    pub announced: RefCell<Vec<HarnessId>>,
    pub dismissed: Cell<usize>,
    pub updater_installs: Cell<bool>,
    /// The `force` flag of every check, in the order they started.
    pub checks: RefCell<Vec<bool>>,
    /// The harnesses whose updater ran, in order.
    pub updates: RefCell<Vec<HarnessId>>,
    /// While set, a check or update waits for its sender to fire.
    pub hold_checks: Cell<bool>,
    pub hold_updates: Cell<bool>,
    held: RefCell<Vec<oneshot::Sender<()>>>,
    listeners: RefCell<Vec<Listener>>,
}

impl Default for FakeUpdates {
    fn default() -> Self {
        Self {
            installed: RefCell::new(
                [(HarnessId::Claude, "2.1.284 (Claude Code)".to_string())].into(),
            ),
            latest: RefCell::new([(HarnessId::Claude, "2.1.285".to_string())].into()),
            refreshed: RefCell::default(),
            announced: RefCell::default(),
            dismissed: Cell::new(0),
            updater_installs: Cell::new(true),
            checks: RefCell::default(),
            updates: RefCell::default(),
            hold_checks: Cell::new(false),
            hold_updates: Cell::new(false),
            held: RefCell::default(),
            listeners: RefCell::default(),
        }
    }
}

impl FakeUpdates {
    /// Another window says it updated `harness`.
    fn broadcast(&self, harness: HarnessId, cx: &mut App) {
        let listeners: Vec<Listener> = self.listeners.borrow().clone();
        for listener in listeners {
            listener(harness, cx);
        }
    }

    fn set(&self, map: &RefCell<HashMap<HarnessId, String>>, harness: HarnessId, value: &str) {
        map.borrow_mut().insert(harness, value.to_string());
    }

    /// Lets every held check and update finish.
    fn release(&self) {
        for send in self.held.borrow_mut().drain(..) {
            send.send(()).ok();
        }
    }

    fn gate(&self, hold: bool) -> Option<oneshot::Receiver<()>> {
        hold.then(|| {
            let (send, receive) = oneshot::channel();
            self.held.borrow_mut().push(send);
            receive
        })
    }

    fn version_checks(&self) -> Vec<HarnessVersionCheck> {
        let installed = self.installed.borrow();
        let latest = self.latest.borrow();
        let mut harnesses: Vec<HarnessId> = installed.keys().copied().collect();
        harnesses.sort();
        harnesses
            .into_iter()
            .map(|harness| {
                let installed = parse_harness_version(harness, &installed[&harness]).unwrap();
                let latest = parse_harness_version(harness, &latest[&harness]).unwrap();
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
            })
            .collect()
    }
}

impl HarnessUpdateHost for FakeUpdates {
    fn claim_launch_check(&self, _: &mut App) -> bool {
        true
    }

    fn check_versions(&self, force: bool, cx: &mut App) -> Task<Vec<HarnessVersionCheck>> {
        self.checks.borrow_mut().push(force);
        let checks = self.version_checks();
        let gate = self.gate(self.hold_checks.get());
        cx.foreground_executor().spawn(async move {
            if let Some(gate) = gate {
                gate.await.ok();
            }
            checks
        })
    }

    fn dismiss_updates(&self, _: &mut App) {
        self.dismissed.set(self.dismissed.get() + 1);
    }

    fn update_cli(&self, harness: HarnessId, cx: &mut App) -> HostTask<()> {
        self.updates.borrow_mut().push(harness);
        if self.updater_installs.get() {
            let latest = self.latest.borrow()[&harness].clone();
            self.set(&self.installed, harness, &latest);
        }
        let gate = self.gate(self.hold_updates.get());
        cx.foreground_executor().spawn(async move {
            if let Some(gate) = gate {
                gate.await.ok();
            }
            Ok(())
        })
    }

    fn installed_version(&self, harness: HarnessId, _: &mut App) -> HostTask<Option<String>> {
        Task::ready(Ok(self.installed.borrow().get(&harness).cloned()))
    }

    fn refresh_catalogs(&self, harness: HarnessId, _: &mut App) -> Task<()> {
        self.refreshed.borrow_mut().push(harness);
        Task::ready(())
    }

    fn announce_updated(&self, harness: HarnessId, _: &mut App) {
        self.announced.borrow_mut().push(harness);
    }

    fn on_harness_updated(&self, on_update: OnHarnessUpdated, _: &mut App) -> Option<Subscription> {
        self.listeners.borrow_mut().push(Rc::from(on_update));
        None
    }
}

fn mount_notice(
    cx: &mut TestAppContext,
    host: Rc<FakeUpdates>,
) -> (Entity<HarnessUpdateNotice>, &'static mut VisualTestContext) {
    mount(cx, 900., 600., move |_, cx| {
        cx.new(|cx| HarnessUpdateNotice::new(host, 12., cx))
    })
}

fn store(host: &Rc<FakeUpdates>, cx: &mut TestAppContext) -> Entity<HarnessUpdateStore> {
    init(cx);
    let host = host.clone();
    cx.update(|cx| HarnessUpdateStore::global(host, cx))
}

fn claude_update(latest: &str) -> HarnessUpdate {
    HarnessUpdate {
        harness: HarnessId::Claude,
        installed: "2.1.284".into(),
        latest: latest.into(),
    }
}

fn claude_check(
    cx: &mut TestAppContext,
    store: &Entity<HarnessUpdateStore>,
) -> HarnessVersionCheck {
    store.read_with(cx, |store, _| {
        store
            .checks()
            .unwrap()
            .iter()
            .find(|check| check.harness() == HarnessId::Claude)
            .cloned()
            .unwrap()
    })
}

#[gpui::test]
fn updates_from_the_card_and_refreshes_models_for_other_windows(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    let (notice, cx) = mount_notice(cx, host.clone());
    assert!(exists(cx, "status:Harness updates"));
    assert!(exists(cx, "text:Claude Code"));
    assert!(exists(cx, "text:2.1.284 → 2.1.285"));
    assert!(exists(cx, "text:Harness update available"));

    click(cx, "button:Update:claude");
    assert!(exists(cx, "text:Updated to 2.1.285"));
    assert_eq!(host.refreshed.borrow().as_slice(), [HarnessId::Claude]);
    assert_eq!(host.announced.borrow().as_slice(), [HarnessId::Claude]);
    assert!(exists(
        cx,
        "text:Model picker refreshed with the new version’s models."
    ));

    let broadcast = host.clone();
    cx.update(|_, cx| broadcast.broadcast(HarnessId::Claude, cx));
    assert_eq!(
        host.refreshed.borrow().as_slice(),
        [HarnessId::Claude, HarnessId::Claude]
    );
    assert_eq!(
        notice.read_with(cx, |notice, cx| notice.row(HarnessId::Claude, cx)),
        RowState::Updated("2.1.285".into())
    );
}

#[gpui::test]
fn reports_an_updater_that_left_the_old_version_and_can_be_dismissed(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    host.updater_installs.set(false);
    let (notice, cx) = mount_notice(cx, host.clone());
    click(cx, "button:Update:claude");
    assert!(exists(cx, "text:Still on 2.1.284 after updating."));
    assert!(exists(cx, "button:Retry:claude"));
    assert!(host.refreshed.borrow().is_empty());
    click(cx, "button:Dismiss harness updates");
    assert_eq!(host.dismissed.get(), 1);
    assert!(notice.read_with(cx, |notice, _| notice.updates().is_empty()));
    assert!(!exists(cx, "status:Harness updates"));
}

#[gpui::test]
fn leaves_harnesses_hidden_from_the_picker_out_of_the_notice(cx: &mut TestAppContext) {
    struct HiddenClaude(Rc<FakeUpdates>);
    impl HarnessUpdateHost for HiddenClaude {
        fn claim_launch_check(&self, _: &mut App) -> bool {
            true
        }
        fn check_versions(&self, force: bool, cx: &mut App) -> Task<Vec<HarnessVersionCheck>> {
            self.0.check_versions(force, cx)
        }
        fn is_picker_visible(&self, harness: HarnessId, _: &App) -> bool {
            harness != HarnessId::Claude
        }
    }
    let host = Rc::new(FakeUpdates::default());
    host.set(&host.installed, HarnessId::Codex, "codex-cli 0.159.1");
    host.set(&host.latest, HarnessId::Codex, "0.159.2");
    let hidden = Rc::new(HiddenClaude(host.clone()));
    let (notice, cx) = mount(cx, 900., 600., move |_, cx| {
        cx.new(|cx| HarnessUpdateNotice::new(hidden, 12., cx))
    });
    assert_eq!(
        notice.read_with(cx, |notice, _| notice
            .updates()
            .iter()
            .map(|update| update.harness)
            .collect::<Vec<_>>()),
        [HarnessId::Codex]
    );
    // Settings still lists the hidden harness.
    let listed = cx.update(|_, cx| {
        HarnessUpdateStore::global(host.clone(), cx)
            .read(cx)
            .checks()
            .map(<[_]>::len)
    });
    assert_eq!(listed, Some(2));
}

#[gpui::test]
fn a_second_request_waits_on_the_running_update(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    host.hold_updates.set(true);
    let store = store(&host, cx);
    store.update(cx, |store, cx| {
        store.run_update(claude_update("2.1.285"), cx);
        store.run_update(claude_update("2.1.285"), cx);
    });
    cx.run_until_parked();
    assert_eq!(host.updates.borrow().as_slice(), [HarnessId::Claude]);
    assert_eq!(
        store.read_with(cx, |store, _| store.run(HarnessId::Claude)),
        RowState::Updating
    );
    host.release();
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _| store.run(HarnessId::Claude)),
        RowState::Updated("2.1.285".into())
    );
}

#[gpui::test]
fn a_forced_check_runs_after_the_running_one(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    host.hold_checks.set(true);
    let store = store(&host, cx);
    let (first, shared, forced) = store.update(cx, |store, cx| {
        (
            store.check(false, cx),
            store.check(false, cx),
            store.check(true, cx),
        )
    });
    cx.run_until_parked();
    assert_eq!(host.checks.borrow().as_slice(), [false]);
    host.release();
    cx.run_until_parked();
    assert_eq!(host.checks.borrow().as_slice(), [false, true]);
    host.release();
    cx.run_until_parked();
    for task in [first, shared, forced] {
        assert_eq!(futures::executor::block_on(task).len(), 1);
    }
    assert!(!store.read_with(cx, |store, _| store.checking()));
}

#[gpui::test]
fn a_check_that_overlaps_an_update_keeps_the_updated_version(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    let store = store(&host, cx);
    // The check reads 2.1.284 and is still waiting on the feed when the
    // update finishes.
    host.hold_checks.set(true);
    store.update(cx, |store, cx| store.check(false, cx).detach());
    cx.run_until_parked();
    store.update(cx, |store, cx| {
        store.run_update(claude_update("2.1.285"), cx)
    });
    cx.run_until_parked();
    host.release();
    cx.run_until_parked();
    assert_eq!(
        claude_check(cx, &store),
        HarnessVersionCheck::Current {
            harness: HarnessId::Claude,
            installed: "2.1.285".into(),
            latest: "2.1.285".into(),
        }
    );

    // A check that found a newer release while the update ran keeps that
    // release, so the harness is still behind.
    host.set(&host.latest, HarnessId::Claude, "2.1.287");
    store.update(cx, |store, cx| store.check(false, cx).detach());
    cx.run_until_parked();
    // The update was offered 2.1.286 and installs it.
    host.set(&host.latest, HarnessId::Claude, "2.1.286");
    store.update(cx, |store, cx| {
        store.run_update(claude_update("2.1.286"), cx)
    });
    cx.run_until_parked();
    host.release();
    cx.run_until_parked();
    assert_eq!(
        claude_check(cx, &store),
        HarnessVersionCheck::Behind {
            harness: HarnessId::Claude,
            installed: "2.1.286".into(),
            latest: "2.1.287".into(),
        }
    );
}

#[gpui::test]
fn an_update_that_keeps_the_same_cursor_build_fails(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    host.set(&host.installed, HarnessId::Cursor, "2026.09.28-9a7762b");
    host.set(&host.latest, HarnessId::Cursor, "2026.09.28-64d2043");
    host.updater_installs.set(false);
    let store = store(&host, cx);
    store.update(cx, |store, cx| {
        store.run_update(
            HarnessUpdate {
                harness: HarnessId::Cursor,
                installed: "2026.09.28-9a7762b".into(),
                latest: "2026.09.28-64d2043".into(),
            },
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(
        store.read_with(cx, |store, _| store.run(HarnessId::Cursor)),
        RowState::Failed("Still on 2026.09.28-9a7762b after updating.".into())
    );
}

#[gpui::test]
fn checks_cli_versions_on_request_and_updates_from_settings(cx: &mut TestAppContext) {
    let host = Rc::new(FakeUpdates::default());
    host.set(&host.installed, HarnessId::Cursor, "2026.09.28-64d2043");
    host.set(&host.latest, HarnessId::Cursor, "2026.09.28-64d2043");
    let card_host = host.clone();
    let (card, cx) = mount(cx, 900., 600., move |_, cx| {
        cx.new(|cx| HarnessUpdatesCard::new(card_host, cx))
    });
    let store = cx.update(|_, cx| HarnessUpdateStore::global(host.clone(), cx));
    assert!(exists(cx, "setting-id:harness-updates"));
    // Opening the page runs no CLI.
    assert!(host.checks.borrow().is_empty());
    assert!(store.read_with(cx, |store, _| store.checks().is_none()));

    click(cx, "button:Check for updates");
    assert_eq!(host.checks.borrow().as_slice(), [true]);
    assert!(exists(cx, "harness-update:claude"));
    assert!(exists(cx, "harness-update:cursor"));
    assert_eq!(
        row_description(&claude_check(cx, &store), &RowState::Idle),
        "Version 2.1.285 is available."
    );

    click(cx, "button:Update Claude Code to 2.1.285");
    assert_eq!(host.updates.borrow().as_slice(), [HarnessId::Claude]);
    let state = store.read_with(cx, |store, _| store.run(HarnessId::Claude));
    assert_eq!(
        row_description(&claude_check(cx, &store), &state),
        "Updated to 2.1.285."
    );
    assert!(!exists(cx, "button:Update Claude Code to 2.1.285"));

    // Claude Code, updated earlier in this session, falls behind again and
    // still counts for Update all.
    host.set(&host.latest, HarnessId::Claude, "2.1.286");
    host.set(&host.latest, HarnessId::Cursor, "2026.09.29-1234567");
    click(cx, "button:Check for updates");
    assert_eq!(card.read_with(cx, |card, cx| card.pending(cx).len()), 2);
    host.updates.borrow_mut().clear();
    click(cx, "button:Update all");
    let mut updated = host.updates.borrow().clone();
    updated.sort();
    assert_eq!(updated, [HarnessId::Claude, HarnessId::Cursor]);
}

#[test]
fn describes_each_row_state() {
    let current = HarnessVersionCheck::Current {
        harness: HarnessId::Grok,
        installed: "1.0.46".into(),
        latest: "1.0.46".into(),
    };
    let behind = HarnessVersionCheck::Behind {
        harness: HarnessId::Grok,
        installed: "1.0.45".into(),
        latest: "1.0.46".into(),
    };
    let unknown = HarnessVersionCheck::Unknown {
        harness: HarnessId::Grok,
        error: "offline".into(),
    };
    assert_eq!(row_description(&current, &RowState::Idle), "Up to date.");
    assert_eq!(row_description(&behind, &RowState::Updating), "Updating…");
    assert_eq!(
        row_description(&behind, &RowState::Failed("no write access".into())),
        "no write access"
    );
    assert_eq!(
        row_description(&unknown, &RowState::Idle),
        "Could not check: offline"
    );
}
