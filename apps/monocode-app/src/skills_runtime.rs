//! Shared skill preparation for local provider launches and composer catalogs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use monocode_engine::submit::skills::SkillCatalogContext;
use monocode_process::harness::{HarnessHost, SkillLaunchContext};
use monocode_skills::{ExportTarget, SkillManager};

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
        Ok(())
    })));
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::HarnessId;

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
