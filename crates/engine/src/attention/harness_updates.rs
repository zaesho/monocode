//! Port of src/features/providers/model/harnessUpdates.ts: the launch check
//! for harness CLI updates and the "harness updated" broadcast.
//!
//! Tauri sent `harness-updated` to every window with a per-window source id
//! so the sender skipped its own event. Here the windows share one app, so
//! the broadcast is a GPUI event on the `HarnessUpdates` entity and each
//! listener passes its own source id.

use std::sync::LazyLock;

use futures::future::{BoxFuture, join_all};
use gpui::{App, AppContext, Context, Entity, EventEmitter, Subscription};
use monocode_core::HarnessId;
use monocode_harness::providers::opencode::protocol::{compare_semver, parse_open_code_version};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// `UPDATABLE_HARNESSES`: harnesses with a release feed and a self-updater
/// MonoCode can run.
pub const UPDATABLE_HARNESSES: [HarnessId; 8] = [
    HarnessId::Claude,
    HarnessId::Codex,
    HarnessId::Cursor,
    HarnessId::Grok,
    HarnessId::Opencode,
    HarnessId::Pi,
    HarnessId::Omp,
    HarnessId::Fx,
];

/// `HarnessUpdate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessUpdate {
    pub harness: HarnessId,
    pub installed: String,
    pub latest: String,
}

/// `HarnessVersionCheck`: one harness compared with its newest release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum HarnessVersionCheck {
    Current {
        harness: HarnessId,
        installed: String,
        latest: String,
    },
    Behind {
        harness: HarnessId,
        installed: String,
        latest: String,
    },
    /// The CLI or its feed gave no version, or a lookup failed.
    Unknown { harness: HarnessId, error: String },
}

impl HarnessVersionCheck {
    pub fn harness(&self) -> HarnessId {
        match self {
            Self::Current { harness, .. }
            | Self::Behind { harness, .. }
            | Self::Unknown { harness, .. } => *harness,
        }
    }
}

/// `installedVersion`: the harness's `--version` output, if installed.
pub type InstalledVersion =
    Box<dyn Fn(HarnessId) -> BoxFuture<'static, Result<Option<String>, String>>>;

/// `latestVersion`: the newest published release.
pub type LatestVersion = Box<dyn Fn(HarnessId) -> BoxFuture<'static, Result<String, String>>>;

/// `HarnessUpdateDeps`.
pub struct HarnessUpdateDeps {
    /// Installed harnesses to check.
    pub harnesses: Vec<HarnessId>,
    pub installed_version: InstalledVersion,
    pub latest_version: LatestVersion,
}

/// Cursor names a build by date and commit, such as `2026.09.28-64d2043`.
static CURSOR_BUILD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}\.\d{2}\.\d{2}-[0-9a-f]+").expect("valid regex"));

/// `parseHarnessVersion`: the version to compare and display. Cursor keeps
/// its full build, because two builds can share a date.
pub fn parse_harness_version(harness: HarnessId, output: &str) -> Option<String> {
    if harness == HarnessId::Cursor
        && let Some(build) = CURSOR_BUILD.find(output)
    {
        return Some(build.as_str().to_string());
    }
    parse_open_code_version(output)
}

/// `isHarnessVersionBehind`: true when `installed` is older than `latest`.
/// A Cursor build from the same day as the feed but with another commit is
/// behind, since the feed names the newest build. Commit hashes have no
/// order, so this is the only way to tell.
pub fn is_harness_version_behind(harness: HarnessId, installed: &str, latest: &str) -> bool {
    let order = compare_semver(latest, installed);
    if order != 0 {
        return order > 0;
    }
    harness == HarnessId::Cursor && installed != latest
}

/// `checkHarnessVersions`: compares each harness with its newest release. A
/// failed lookup becomes an `Unknown` entry for that harness instead of
/// failing the whole check. Installed builds newer than the feed, such as a
/// dev channel, count as current.
pub async fn check_harness_versions(deps: HarnessUpdateDeps) -> Vec<HarnessVersionCheck> {
    let checks = deps
        .harnesses
        .iter()
        .filter(|harness| UPDATABLE_HARNESSES.contains(harness))
        .map(|&harness| {
            let installed = (deps.installed_version)(harness);
            let latest = (deps.latest_version)(harness);
            async move {
                let unknown = |error: String| HarnessVersionCheck::Unknown { harness, error };
                let (installed, latest) = futures::join!(installed, latest);
                let installed = match installed {
                    Ok(output) => output,
                    Err(error) => return unknown(error),
                };
                let latest = match latest {
                    Ok(output) => output,
                    Err(error) => return unknown(error),
                };
                let Some(installed) =
                    parse_harness_version(harness, installed.as_deref().unwrap_or(""))
                else {
                    return unknown("The CLI reported no version.".into());
                };
                let Some(latest) = parse_harness_version(harness, &latest) else {
                    return unknown("The release feed returned no version.".into());
                };
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
        });
    join_all(checks).await
}

/// `pendingHarnessUpdates`: only the harnesses behind their newest release.
pub fn pending_harness_updates(checks: &[HarnessVersionCheck]) -> Vec<HarnessUpdate> {
    checks
        .iter()
        .filter_map(|check| match check {
            HarnessVersionCheck::Behind {
                harness,
                installed,
                latest,
            } => Some(HarnessUpdate {
                harness: *harness,
                installed: installed.clone(),
                latest: latest.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// `findHarnessUpdates`. This runs unprompted at launch, so a failed lookup
/// drops that harness silently. Nothing is remembered between launches, so a
/// harness still behind is offered again at whatever release is newest by
/// then.
pub async fn find_harness_updates(deps: HarnessUpdateDeps) -> Vec<HarnessUpdate> {
    pending_harness_updates(&check_harness_versions(deps).await)
}

/// `claimLaunchHarnessUpdateCheck`: true for the first caller per app
/// process, so a window opened later does not repeat the launch check.
pub fn claim_launch_harness_update_check() -> bool {
    monocode_integrations::harness_updates::harness_update_check_claim()
}

/// `fetchLatestHarnessVersion`. Blocks on the network; run it off the UI
/// thread.
pub fn fetch_latest_harness_version(harness: HarnessId) -> Result<String, String> {
    monocode_integrations::harness_updates::harness_latest_version(harness.as_str().to_string())
}

/// `HarnessUpdatedEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessUpdated {
    pub harness: HarnessId,
    /// The window (or other caller) that ran the update and already
    /// refreshed its own catalog.
    pub source: u64,
}

/// The `harness-updated` broadcast.
#[derive(Default)]
pub struct HarnessUpdates {
    next_source: u64,
}

impl EventEmitter<HarnessUpdated> for HarnessUpdates {}

impl HarnessUpdates {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|_| Self::default())
    }

    /// A fresh source id for one window's listener and announcements.
    pub fn new_source(&mut self) -> u64 {
        self.next_source += 1;
        self.next_source
    }

    /// `announceHarnessUpdated`.
    pub fn announce(&mut self, harness: HarnessId, source: u64, cx: &mut Context<Self>) {
        cx.emit(HarnessUpdated { harness, source });
    }
}

/// `onHarnessUpdated`: call `handler` for updates other sources announced.
/// Dropping the subscription stops it.
pub fn on_harness_updated(
    updates: &Entity<HarnessUpdates>,
    source: u64,
    mut handler: impl FnMut(HarnessId, &mut App) + 'static,
    cx: &mut App,
) -> Subscription {
    cx.subscribe(updates, move |_, event: &HarnessUpdated, cx| {
        // The sender awaited its local refresh before announcing the update.
        if event.source != source {
            handler(event.harness, cx);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use gpui::TestAppContext;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    fn installed() -> HashMap<HarnessId, &'static str> {
        [
            (HarnessId::Claude, "2.1.284 (Claude Code)"),
            (HarnessId::Codex, "codex-cli 0.159.2"),
            (HarnessId::Opencode, "1.18.31"),
            (HarnessId::Cursor, "2026.09.01-abc"),
        ]
        .into_iter()
        .collect()
    }

    fn deps(
        harnesses: &[HarnessId],
        installed: HashMap<HarnessId, &'static str>,
        latest: HashMap<HarnessId, &'static str>,
    ) -> HarnessUpdateDeps {
        HarnessUpdateDeps {
            harnesses: harnesses.to_vec(),
            installed_version: Box::new(move |id| {
                let version = installed.get(&id).map(|v| v.to_string());
                async move { Ok(version) }.boxed()
            }),
            latest_version: Box::new(move |id| {
                let version = latest.get(&id).map(|v| v.to_string());
                async move { version.ok_or_else(|| "registry unreachable".to_string()) }.boxed()
            }),
        }
    }

    fn find(
        harnesses: &[HarnessId],
        latest: HashMap<HarnessId, &'static str>,
    ) -> Vec<HarnessUpdate> {
        smol::block_on(find_harness_updates(deps(harnesses, installed(), latest)))
    }

    fn latest() -> HashMap<HarnessId, &'static str> {
        [
            (HarnessId::Claude, "2.1.285"),
            (HarnessId::Codex, "0.159.2"),
            (HarnessId::Opencode, "1.18.33"),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn reports_only_harnesses_behind_the_published_release() {
        assert_eq!(
            find(
                &[HarnessId::Claude, HarnessId::Codex, HarnessId::Opencode],
                latest()
            ),
            vec![
                HarnessUpdate {
                    harness: HarnessId::Claude,
                    installed: "2.1.284".into(),
                    latest: "2.1.285".into(),
                },
                HarnessUpdate {
                    harness: HarnessId::Opencode,
                    installed: "1.18.31".into(),
                    latest: "1.18.33".into(),
                },
            ]
        );
    }

    #[test]
    fn skips_harnesses_without_a_version_feed_or_whose_lookup_fails() {
        assert_eq!(
            find(
                &[
                    HarnessId::Cursor,
                    HarnessId::Pi,
                    HarnessId::Hermes,
                    HarnessId::Antigravity
                ],
                latest()
            ),
            vec![]
        );
    }

    #[test]
    fn reports_each_harness_as_behind_current_or_unknown() {
        let installed = [
            (HarnessId::Cursor, "2026.09.18-9a7762b"),
            (HarnessId::Grok, "grok 1.0.46 (4220f3b224a6) [stable]"),
            (HarnessId::Fx, "fx v0.0.13-e3ad6d8 [dev]"),
            (HarnessId::Omp, "omp/18.4.8"),
            (HarnessId::Pi, "no version here"),
        ]
        .into_iter()
        .collect();
        let latest = [
            (HarnessId::Cursor, "2026.09.28-64d2043"),
            (HarnessId::Grok, "1.0.46"),
            (HarnessId::Fx, "v0.0.12"),
            (HarnessId::Pi, "0.80.0"),
        ]
        .into_iter()
        .collect();
        let checks = smol::block_on(check_harness_versions(deps(
            &[
                HarnessId::Cursor,
                HarnessId::Grok,
                HarnessId::Fx,
                HarnessId::Omp,
                HarnessId::Pi,
                HarnessId::Hermes,
            ],
            installed,
            latest,
        )));
        assert_eq!(
            checks,
            vec![
                HarnessVersionCheck::Behind {
                    harness: HarnessId::Cursor,
                    installed: "2026.09.18-9a7762b".into(),
                    latest: "2026.09.28-64d2043".into(),
                },
                HarnessVersionCheck::Current {
                    harness: HarnessId::Grok,
                    installed: "1.0.46".into(),
                    latest: "1.0.46".into(),
                },
                // A dev build ahead of the stable feed is not offered a
                // downgrade.
                HarnessVersionCheck::Current {
                    harness: HarnessId::Fx,
                    installed: "0.0.13".into(),
                    latest: "0.0.12".into(),
                },
                HarnessVersionCheck::Unknown {
                    harness: HarnessId::Omp,
                    error: "registry unreachable".into(),
                },
                HarnessVersionCheck::Unknown {
                    harness: HarnessId::Pi,
                    error: "The CLI reported no version.".into(),
                },
            ]
        );
    }

    #[test]
    fn compares_cursor_builds_from_the_same_day_by_their_full_build() {
        let cursor = HarnessId::Cursor;
        assert!(is_harness_version_behind(
            cursor,
            "2026.09.28-9a7762b",
            "2026.09.28-64d2043"
        ));
        assert!(!is_harness_version_behind(
            cursor,
            "2026.09.28-64d2043",
            "2026.09.28-64d2043"
        ));
        assert!(!is_harness_version_behind(
            cursor,
            "2026.10.01-1111111",
            "2026.09.28-64d2043"
        ));
        // Other harnesses compare release numbers only.
        assert!(!is_harness_version_behind(
            HarnessId::Grok,
            "1.0.46",
            "1.0.46"
        ));
    }

    #[test]
    fn offers_a_harness_still_behind_again_at_the_newest_release() {
        assert_eq!(find(&[HarnessId::Claude], latest())[0].latest, "2.1.285");
        let mut newer = latest();
        newer.insert(HarnessId::Claude, "2.1.286");
        assert_eq!(
            find(&[HarnessId::Claude], newer),
            vec![HarnessUpdate {
                harness: HarnessId::Claude,
                installed: "2.1.284".into(),
                latest: "2.1.286".into(),
            }]
        );
    }

    #[gpui::test]
    fn delivers_updates_to_other_windows_while_skipping_the_sender(cx: &mut TestAppContext) {
        let updates = cx.update(HarnessUpdates::new);
        let local_seen = Rc::new(RefCell::new(Vec::new()));
        let remote_seen = Rc::new(RefCell::new(Vec::new()));
        let (local, remote) = updates.update(cx, |updates, _| {
            (updates.new_source(), updates.new_source())
        });
        let stop_local = cx.update(|cx| {
            let seen = local_seen.clone();
            on_harness_updated(
                &updates,
                local,
                move |harness, _| seen.borrow_mut().push(harness),
                cx,
            )
        });
        let _stop_remote = cx.update(|cx| {
            let seen = remote_seen.clone();
            on_harness_updated(
                &updates,
                remote,
                move |harness, _| seen.borrow_mut().push(harness),
                cx,
            )
        });

        updates.update(cx, |updates, cx| {
            updates.announce(HarnessId::Claude, local, cx)
        });
        cx.run_until_parked();
        assert!(local_seen.borrow().is_empty());
        assert_eq!(*remote_seen.borrow(), vec![HarnessId::Claude]);

        updates.update(cx, |updates, cx| {
            updates.announce(HarnessId::Codex, remote, cx)
        });
        cx.run_until_parked();
        assert_eq!(*local_seen.borrow(), vec![HarnessId::Codex]);
        assert_eq!(remote_seen.borrow().len(), 1);

        drop(stop_local);
        updates.update(cx, |updates, cx| {
            updates.announce(HarnessId::Opencode, remote, cx)
        });
        cx.run_until_parked();
        assert_eq!(local_seen.borrow().len(), 1);
        assert_eq!(remote_seen.borrow().len(), 1);
    }
}
