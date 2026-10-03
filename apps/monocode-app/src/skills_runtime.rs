//! Shared skill preparation for local provider launches and composer catalogs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use monocode_engine::submit::skills::SkillCatalogContext;
use monocode_process::harness::{HarnessHost, SkillLaunchContext, lock_skill_account_lifecycle};
use monocode_skills::{ExportState, ExportTarget, ReconcileReport, SkillManager};

pub fn initialize_manager(
    data_dir: &Path,
    home: &Path,
) -> Result<(Arc<SkillManager>, u64), String> {
    let _lifecycle = lock_skill_account_lifecycle(data_dir)?;
    let manager = Arc::new(SkillManager::open(data_dir, home).map_err(|error| error.to_string())?);
    let report = manager.reconcile(&[]).map_err(|error| error.to_string())?;
    if let Some(warning) = pending_warning(&report) {
        log::warn!("Shared skill exports need repair: {warning}");
    }
    Ok((manager, report.generation))
}

fn pending_warning(report: &ReconcileReport) -> Option<String> {
    let warnings = report
        .statuses
        .iter()
        .filter(|status| status.export.state == ExportState::Pending)
        .map(|status| format!("{}: {}", status.export.path.display(), status.export.detail))
        .collect::<Vec<_>>();
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

/// Resolve the same account directories the process supervisor uses.
pub fn resolve_context(
    context: SkillCatalogContext,
    data_dir: &Path,
    skill_home: &Path,
    generation: u64,
    isolated: bool,
) -> SkillCatalogContext {
    let mut context = context
        .with_home(skill_home.to_string_lossy().into_owned())
        .with_library_generation(generation);
    let provider = context.harness.as_str();
    if provider != "claude" && provider != "codex" {
        return context;
    }
    let named = context
        .account_id
        .as_deref()
        .filter(|id| *id != "default")
        .and_then(|id| {
            monocode_process::harness::provider_account_path(data_dir, provider, id).ok()
        })
        .and_then(|path| {
            monocode_process::harness::resolve_provider_home(&path, Path::new(&context.cwd)).ok()
        });
    let root = named.or_else(|| {
        if isolated {
            return None;
        }
        let key = if provider == "claude" {
            "CLAUDE_CONFIG_DIR"
        } else {
            "CODEX_HOME"
        };
        std::env::var_os(key)
            .filter(|value| !value.is_empty())
            .and_then(|value| {
                monocode_process::harness::resolve_provider_home(
                    Path::new(&value),
                    Path::new(&context.cwd),
                )
                .ok()
            })
    });
    let root = root.filter(|root| {
        !isolated
            || monocode_process::harness::resolve_provider_home(data_dir, Path::new(&context.cwd))
                .is_ok_and(|data| root.starts_with(data))
    });
    if let Some(root) = root {
        context = context.with_provider_home(provider, root.to_string_lossy().into_owned());
    }
    context
}

/// Export only skill bundles to already resolved configuration directories.
pub fn account_targets(context: &SkillLaunchContext) -> Vec<ExportTarget> {
    context
        .provider_homes
        .iter()
        .map(|(provider, home)| {
            ExportTarget::new(
                format!("config:{provider}:{}", home.to_string_lossy()),
                home.join("skills"),
                vec![provider.clone()],
            )
        })
        .collect()
}

pub fn install_preparer(
    host: &HarnessHost,
    manager: Arc<SkillManager>,
    generation: Arc<AtomicU64>,
    isolated_data_dir: Option<PathBuf>,
) {
    let retiring_manager = manager.clone();
    let retiring_generation = generation.clone();
    host.set_skill_account_retirer(Some(Arc::new(move |root| {
        let report = retiring_manager
            .retire_targets_under(&root)
            .map_err(|error| error.to_string())?;
        retiring_generation.fetch_max(report.generation, Ordering::AcqRel);
        Ok(())
    })));
    host.set_skill_preparer(Some(Arc::new(move |mut context| {
        if let Some(root) = &isolated_data_dir {
            context
                .provider_homes
                .retain(|_, home| home.starts_with(root));
        }
        let report = manager
            .reconcile(&account_targets(&context))
            .map_err(|error| error.to_string())?;
        generation.fetch_max(report.generation, Ordering::AcqRel);
        match pending_warning(&report) {
            Some(warning) => Err(warning),
            None => Ok(()),
        }
    })));
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::HarnessId;

    fn remove_fixture(root: &Path) {
        fn writable(path: &Path) {
            let metadata = std::fs::symlink_metadata(path).unwrap();
            if metadata.file_type().is_symlink() {
                return;
            }
            let mut permissions = metadata.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.is_dir() {
                    permissions.set_mode(permissions.mode() | 0o700);
                    std::fs::set_permissions(path, permissions).unwrap();
                }
            }
            #[cfg(not(unix))]
            #[allow(clippy::permissions_set_readonly_false)]
            {
                permissions.set_readonly(false);
                std::fs::set_permissions(path, permissions).unwrap();
            }
            if metadata.is_dir() {
                for child in std::fs::read_dir(path).unwrap() {
                    writable(&child.unwrap().path());
                }
            }
        }
        writable(root);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_registry_returns_an_error_without_replacing_its_bytes() {
        let root = std::env::temp_dir().join(format!("mc-skill-startup-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let home = root.join("home");
        std::fs::create_dir_all(data.join("skills")).unwrap();
        let registry = data.join("skills/registry.json");
        std::fs::write(&registry, "Invalid registry").unwrap();
        assert!(initialize_manager(&data, &home).is_err());
        assert_eq!(
            std::fs::read_to_string(registry).unwrap(),
            "Invalid registry"
        );
        remove_fixture(&root);
    }

    #[test]
    fn preparation_preserves_pending_export_failure_details_as_warnings() {
        let root = std::env::temp_dir().join(format!("mc-skill-warning-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let home = root.join("home");
        let (manager, _) = initialize_manager(&data, &home).unwrap();
        let source = root.join("review");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: Review files\n---\nReview instructions\n",
        )
        .unwrap();
        manager.import(&source).unwrap();
        let blocked = home.join(".claude/skills");
        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::write(&blocked, "Preserve this file").unwrap();
        let report = manager.reconcile(&[]).unwrap();
        let warning = pending_warning(&report).unwrap();
        assert!(warning.contains("Cannot prepare export directory:"));
        assert!(warning.contains(&blocked.to_string_lossy().to_string()));
        assert_eq!(
            std::fs::read_to_string(blocked).unwrap(),
            "Preserve this file"
        );
        remove_fixture(&root);
    }

    #[test]
    fn lifecycle_guard_orders_captured_target_reconciliation_before_account_removal() {
        use std::sync::mpsc;
        use std::time::Duration;

        let root =
            std::env::temp_dir().join(format!("mc-skill-lifecycle-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let home = root.join("home");
        let (manager, _) = initialize_manager(&data, &home).unwrap();
        let source = root.join("review");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: Review files\n---\nReview instructions\n",
        )
        .unwrap();
        manager.import(source).unwrap();
        let profile = data.join("provider-accounts/codex/work");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("auth.json"), "test account artifact").unwrap();
        let host = HarnessHost::default();
        install_preparer(&host, manager.clone(), Arc::new(AtomicU64::new(0)), None);

        let lifecycle = lock_skill_account_lifecycle(&data).unwrap();
        let captured = account_targets(&SkillLaunchContext {
            provider: Some("codex".into()),
            provider_homes: [("codex".into(), profile.clone())].into(),
            ..Default::default()
        });
        let removal_data = data.clone();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let removal = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            monocode_process::harness::provider_account_remove(
                &host,
                &removal_data,
                "codex".into(),
                "work".into(),
            )
            .unwrap();
            done_tx.send(()).unwrap();
        });
        attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        manager.reconcile(&captured).unwrap();
        assert!(profile.join("skills/review/SKILL.md").exists());
        drop(lifecycle);
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        removal.join().unwrap();
        assert!(!profile.exists());

        let later = lock_skill_account_lifecycle(&data).unwrap();
        manager.reconcile(&[]).unwrap();
        assert!(!profile.exists());
        drop(later);
        remove_fixture(&root);
    }

    #[test]
    fn catalog_uses_selected_account_and_isolated_home_without_creating_profiles() {
        let data = std::env::temp_dir().join(format!("mc-skill-context-{}", uuid::Uuid::new_v4()));
        let home = data.join("preview-home");
        let context = resolve_context(
            SkillCatalogContext::new(HarnessId::Claude, "/project").with_account("work"),
            &data,
            &home,
            7,
            true,
        );
        assert_eq!(context.account_id.as_deref(), Some("work"));
        assert_eq!(context.library_generation, 7);
        assert_eq!(
            context.home.as_deref(),
            Some(home.to_string_lossy().as_ref())
        );
        assert_eq!(
            context.provider_homes["claude"],
            monocode_process::harness::resolve_provider_home(
                &data.join("provider-accounts/claude/work"),
                Path::new("/project"),
            )
            .unwrap()
            .to_string_lossy()
        );
        assert!(!data.exists());
    }

    #[test]
    fn provider_export_targets_contain_only_skill_directories() {
        let root = std::env::temp_dir().join("profile with spaces");
        let context = SkillLaunchContext {
            provider: Some("codex".into()),
            provider_homes: [("codex".into(), root.clone())].into(),
            ..Default::default()
        };
        let targets = account_targets(&context);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].root, root.join("skills"));
        assert_eq!(targets[0].providers, vec!["codex"]);
    }

    #[cfg(unix)]
    #[test]
    fn isolated_catalog_rejects_a_profile_symlink_outside_preview_data() {
        let root =
            std::env::temp_dir().join(format!("mc-skill-isolation-{}", uuid::Uuid::new_v4()));
        let data = root.join("data");
        let external = root.join("external");
        let profiles = data.join("provider-accounts/claude");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, profiles.join("work")).unwrap();
        let context = resolve_context(
            SkillCatalogContext::new(HarnessId::Claude, "/project").with_account("work"),
            &data,
            &root.join("home"),
            1,
            true,
        );
        assert!(context.provider_homes.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
