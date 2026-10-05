//! The engine behind one session's composer: `ComposerHost` over `Submit`
//! (turns, stop, drafts, compaction, skills, attachments), side threads for
//! `/btw`, the workspace file index for `@` mentions, and the pickers'
//! `ModelSource` over the live model catalog. Port of the props
//! SessionPane.tsx passed to Composer.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::Shared;
use gpui::{App, AppContext as _, Global, Task, WeakEntity, Window};
use monocode_core::models::{AgentModel, ModelPrefs};
use monocode_core::project_providers::{PROJECT_PROVIDER_SETTINGS_KEY, ProjectProviders};
use monocode_core::{Attachment, HarnessId};
use monocode_engine::history::HistoryPackage;
use monocode_engine::runtime::Engine;
use monocode_engine::submit::attachments::{self, AttachmentIo, PastedFile};
use monocode_engine::submit::mcp_settings_cache::McpSettingsCache;
use monocode_engine::submit::skills::{self as engine_skills, SkillCatalog, SkillCatalogContext};
use monocode_engine::submit::{Submit, SubmitOptions};
use monocode_engine::workspace::Files;
use monocode_engine::workspace::files::file_index::{FAILED_SCAN_RETRY, rank_project_files};
use monocode_harness::core::catalog::SharedCatalog;
use monocode_harness::{HarnessAvailabilityStore, HarnessRegistry, harness_unavailable_hint};
use monocode_settings::Kv;
use monocode_view_composer::composer::model::clipboard::ClipboardFile;
use monocode_view_composer::composer::model::mcp::{McpConnection, McpTag};
use monocode_view_composer::composer::model::mentions::{
    MentionIndex, ProjectFile, RankedFile, build_mention_index,
};
use monocode_view_composer::composer::model::skills::Skill;
use monocode_view_composer::composer::{
    Composer, ComposerHost, ComposerSubmission, FolderTarget, McpServers, NewSkillScope,
    SessionFolder, SkillContext,
};
use monocode_view_composer::pickers::ModelSource;
use monocode_view_transcript::threads::BtwSheet;

use monocode_app::boot::AppServices;

/// The model picker's source: the live catalog and the installer probe.
pub struct CatalogModelSource {
    catalog: SharedCatalog,
    availability: HarnessAvailabilityStore,
    registry: HarnessRegistry,
    /// The working directory whose OpenCode catalog the picker shows.
    project: Option<String>,
}

impl CatalogModelSource {
    pub fn from_services(services: &AppServices) -> Self {
        Self {
            catalog: services.catalog.clone(),
            availability: services.availability.clone(),
            registry: services.registry.clone(),
            project: None,
        }
    }

    /// `localProjectModelSource`: OpenCode reads its models from project
    /// config, so a session shows the catalog of its own working directory.
    pub fn for_project(mut self, project: Option<String>) -> Self {
        self.project = project.filter(|project| !project.is_empty());
        self
    }

    /// The catalog read in this source's project, for a provider that has one.
    fn project_models(&self, harness: HarnessId) -> Option<Vec<AgentModel>> {
        let project = self.project.as_deref()?;
        (harness == HarnessId::Opencode)
            .then(|| {
                self.catalog
                    .read()
                    .project_harness_models(harness, project)
                    .map(<[AgentModel]>::to_vec)
            })
            .flatten()
    }
}

impl ModelSource for CatalogModelSource {
    fn models_for(&self, harness: HarnessId) -> Vec<AgentModel> {
        self.project_models(harness)
            .unwrap_or_else(|| self.catalog.read().models_for(harness).to_vec())
    }

    fn resolve(&self, harness: HarnessId, id: Option<&str>) -> AgentModel {
        self.catalog
            .read()
            .resolve_model_in(harness, id, self.project.as_deref())
    }

    fn find(&self, id: &str) -> Option<AgentModel> {
        if id.starts_with("opencode:") {
            return self
                .models_for(HarnessId::Opencode)
                .into_iter()
                .find(|model| model.id == id);
        }
        self.catalog.read().find_model(id).cloned()
    }

    fn available(&self, harness: HarnessId) -> bool {
        self.availability.is_harness_available(harness)
    }

    fn probed(&self) -> bool {
        self.availability.has_probed_harness_availability()
    }

    /// `refreshHarnessCatalogs` for the tabs the picker shows.
    fn refresh(&self, harnesses: &[HarnessId]) {
        let registry = self.registry.clone();
        let catalog = self.catalog.clone();
        let project = self.project.clone();
        // A project's OpenCode catalog replaces the home one there.
        let opencode_project = project.filter(|_| harnesses.contains(&HarnessId::Opencode));
        let harnesses: Vec<HarnessId> = harnesses
            .iter()
            .copied()
            .filter(|harness| opencode_project.is_none() || *harness != HarnessId::Opencode)
            .collect();
        registry.clone().spawner().spawn(Box::pin(async move {
            if let Some(project) = &opencode_project {
                registry
                    .refresh_project_harness_catalog(HarnessId::Opencode, project)
                    .await;
            }
            registry
                .refresh_harness_catalogs(harnesses, false, |id| catalog.has_live_catalog(id))
                .await;
        }));
    }

    fn unavailable_hint(&self, harness: HarnessId) -> String {
        harness_unavailable_hint(harness)
    }
}

/// `SkillCatalogContext` from the composer's key, with the session's
/// provider account so Claude and Codex read that profile's skills.
fn catalog_context(context: &SkillContext, cx: &App) -> SkillCatalogContext {
    let mut catalog = SkillCatalogContext::new(context.harness, context.cwd.clone());
    if let Some(id) = &context.session_id {
        catalog = catalog.with_session(id.clone());
    }
    if monocode_harness::core::provider_accounts::supports_provider_accounts(context.harness) {
        let account = context
            .session_id
            .as_ref()
            .and_then(|id| session_account(id, cx))
            .or_else(|| {
                let services = AppServices::try_global(cx)?;
                Some(
                    monocode_harness::core::provider_accounts::selected_provider_account_id(
                        &monocode_engine::submit::prefs::KvStore(services.kv.clone()),
                        context.harness,
                        Some(&context.cwd),
                    ),
                )
            });
        if let Some(account) = account {
            catalog = catalog.with_account(account);
        }
    }
    if let Some(submit) = Submit::try_global(cx) {
        catalog = (submit.read(cx).config().skill_context)(catalog);
    }
    catalog
}

/// The composer's slash row for an engine skill.
fn picker_skill(skill: &engine_skills::Skill) -> Skill {
    match skill {
        engine_skills::Skill::File(file) => {
            let scope = match file.scope {
                engine_skills::FileSkillScope::Project => "project",
                engine_skills::FileSkillScope::User => "user",
            };
            let mut row = Skill::file(
                &file.name,
                &file.description,
                &file.path,
                scope,
                &file.source,
            );
            row.invocation = file.invocation.clone();
            row
        }
        engine_skills::Skill::Builtin(builtin) => {
            Skill::builtin(builtin.name, builtin.invocation, builtin.description)
        }
        engine_skills::Skill::Native(native) => {
            let mut row = Skill::native(
                &native.name,
                &native.invocation,
                &native.description,
                native.source.as_str(),
            );
            row.aliases = native.aliases.clone().unwrap_or_default();
            row
        }
    }
}

fn mention_file(file: &monocode_engine::workspace::files::ProjectFile) -> ProjectFile {
    ProjectFile {
        name: file.name.clone(),
        path: file.path.clone(),
        relative: file.relative.clone(),
        is_dir: file.is_dir == Some(true),
    }
}

/// A project listing as the file index shares it.
type Listing = Arc<Vec<monocode_engine::workspace::files::ProjectFile>>;

/// Listings up to this size build their `@` index and rank `@` queries
/// inline. Larger ones do both on the background executor.
const INLINE_LISTING: usize = 2_000;

/// Projects whose `@` index stays built.
const MAX_MENTION_INDEXES: usize = 8;

/// What a project's `@` index was built from.
struct MentionInputs {
    /// The listing, by identity. The file index keeps the same `Arc` while
    /// the files stay the same. A `Weak` keeps the allocation, so the
    /// address cannot be reused while this is held.
    listing: Option<Weak<Vec<monocode_engine::workspace::files::ProjectFile>>>,
    notes: Vec<ProjectFile>,
}

impl MentionInputs {
    fn new(listing: Option<&Listing>, notes: Vec<ProjectFile>) -> Self {
        Self {
            listing: listing.map(Arc::downgrade),
            notes,
        }
    }

    fn matches(&self, listing: Option<&Listing>, notes: &[ProjectFile]) -> bool {
        let same_listing = match (&self.listing, listing) {
            (None, None) => true,
            (Some(built), Some(listing)) => std::ptr::eq(built.as_ptr(), Arc::as_ptr(listing)),
            _ => false,
        };
        same_listing && self.notes == notes
    }
}

struct BuiltMentionIndex {
    inputs: MentionInputs,
    index: Arc<MentionIndex>,
    used: u64,
}

struct MentionIndexBuild {
    id: u64,
    inputs: MentionInputs,
    task: Shared<Task<Arc<MentionIndex>>>,
}

/// The `@` label index of each project, shared by every composer in it.
/// Every session pane used to rebuild it on the UI thread whenever any
/// project's listing changed.
#[derive(Default)]
struct MentionIndexes {
    built: HashMap<String, BuiltMentionIndex>,
    building: HashMap<String, MentionIndexBuild>,
    clock: u64,
}

impl Global for MentionIndexes {}

impl MentionIndexes {
    fn store(&mut self, cwd: String, inputs: MentionInputs, index: Arc<MentionIndex>) {
        self.clock += 1;
        self.built.insert(
            cwd.clone(),
            BuiltMentionIndex {
                inputs,
                index,
                used: self.clock,
            },
        );
        while self.built.len() > MAX_MENTION_INDEXES {
            let Some(oldest) = self
                .built
                .iter()
                .filter(|(key, _)| **key != cwd)
                .min_by_key(|(_, built)| built.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.built.remove(&oldest);
        }
    }
}

fn mention_rows(listing: Option<&Listing>, notes: Vec<ProjectFile>) -> Vec<ProjectFile> {
    let mut files = notes;
    if let Some(listing) = listing {
        files.extend(listing.iter().map(mention_file));
    }
    files
}

fn ranked_file_rows(
    listing: &[monocode_engine::workspace::files::ProjectFile],
    query: &str,
    recents: &[String],
) -> impl Iterator<Item = RankedFile> {
    rank_project_files(listing, query, recents)
        .into_iter()
        .map(|ranked| RankedFile {
            file: mention_file(&ranked.file),
            score: ranked.score,
            positions: ranked.positions,
        })
}

/// One session's `ComposerHost`.
pub struct SessionComposerHost {
    session_id: String,
    kv: Kv,
    skills: Option<SkillCatalog>,
    attachment_io: Option<std::sync::Arc<dyn AttachmentIo>>,
    model_source: Option<Rc<dyn ModelSource>>,
    /// The composer this host serves, for refreshing suggestions when a
    /// skill catalog or the file index finishes loading.
    composer: RefCell<Option<WeakEntity<Composer>>>,
    btw_sheet: RefCell<Option<WeakEntity<BtwSheet>>>,
    registry: Option<HarnessRegistry>,
    mcp_cache: Option<McpSettingsCache>,
    mcp_watches: RefCell<std::collections::HashMap<String, McpWatch>>,
    /// The project whose listing this composer last read. A change to any
    /// other project's listing does not refresh it.
    listing_cwd: RefCell<Option<String>>,
    /// A refresh is already scheduled for when a retry window ends.
    retry_pending: Rc<Cell<bool>>,
}

struct McpWatch {
    cache: McpSettingsCache,
    cwd: String,
    id: u64,
    claude_health: bool,
    _task: Task<()>,
}
impl Drop for McpWatch {
    fn drop(&mut self) {
        self.cache.unsubscribe(&self.cwd, self.id);
    }
}

fn mcp_connection(server: &monocode_engine::submit::mcp::McpConnection) -> McpConnection {
    McpConnection {
        provider: server.provider.as_str().into(),
        name: server.name.clone(),
        scope: server.scope.as_str().into(),
        config_path: server.config_path.clone(),
        transport: server.transport.clone(),
        enabled: server.enabled,
    }
}

/// The provider account a session runs under.
fn session_account(session_id: &str, cx: &App) -> Option<String> {
    monocode_engine::runtime::Engine::try_global(cx)?;
    monocode_engine::runtime::Engine::sessions(cx)
        .read(cx)
        .get(session_id)?
        .provider_account_id
        .clone()
}

fn engine_mcp_connection(
    server: &McpConnection,
) -> Option<monocode_engine::submit::mcp::McpConnection> {
    serde_json::from_value(serde_json::json!({"provider":server.provider,"name":server.name,"scope":server.scope,"configPath":server.config_path,"transport":server.transport,"enabled":server.enabled})).ok()
}

impl SessionComposerHost {
    pub fn new(session_id: String, cx: &App) -> Self {
        let services = AppServices::try_global(cx);
        let submit = Submit::try_global(cx);
        let skills = submit
            .as_ref()
            .map(|submit| submit.read(cx).skills().clone());
        let attachment_io = submit
            .as_ref()
            .map(|submit| submit.read(cx).config().attachment_io.clone());
        let mcp_cache = submit
            .as_ref()
            .and_then(|submit| submit.read(cx).mcp_settings().cloned());
        let model_source = services.map(|services| {
            Rc::new(CatalogModelSource::from_services(services)) as Rc<dyn ModelSource>
        });
        Self {
            session_id,
            kv: services
                .map(|services| services.kv.clone())
                .unwrap_or_else(Kv::in_memory),
            skills,
            attachment_io,
            model_source,
            composer: RefCell::new(None),
            btw_sheet: RefCell::new(None),
            registry: services.map(|services| services.registry.clone()),
            mcp_cache,
            mcp_watches: RefCell::new(std::collections::HashMap::new()),
            listing_cwd: RefCell::new(None),
            retry_pending: Rc::new(Cell::new(false)),
        }
    }

    /// Whether the composer reads the listing of `cwd`.
    pub fn reads_listing_of(&self, cwd: &str) -> bool {
        self.listing_cwd.borrow().as_deref() == Some(cwd)
    }

    pub fn set_composer(&self, composer: WeakEntity<Composer>) {
        *self.composer.borrow_mut() = Some(composer);
    }

    pub fn set_btw_sheet(&self, sheet: WeakEntity<BtwSheet>) {
        *self.btw_sheet.borrow_mut() = Some(sheet);
    }

    /// Ask the composer to re-read skills and mentions once `load` is done.
    fn refresh_when<T: 'static>(&self, load: Task<T>, cx: &mut App) {
        let composer = self.composer.borrow().clone();
        // The side-question composer reads the same caches through this
        // host, so it re-reads them too.
        let sheet = self.btw_sheet.borrow().clone();
        if composer.is_none() && sheet.is_none() {
            load.detach();
            return;
        }
        cx.spawn(async move |cx| {
            load.await;
            if let Some(composer) = composer {
                composer
                    .update(cx, |composer, cx| composer.refresh_suggestions(cx))
                    .ok();
            }
            let side =
                sheet.and_then(|sheet| sheet.read_with(cx, |sheet, _| sheet.composer()).ok());
            if let Some(side) = side.flatten() {
                side.update(cx, |composer, cx| composer.refresh_suggestions(cx));
            }
        })
        .detach();
    }

    /// Refresh the composer once after `delay`, for a cache that answered
    /// from inside its retry window. Calls during the same wait share it.
    fn refresh_after(&self, delay: Duration, cx: &mut App) {
        if self.retry_pending.replace(true) {
            return;
        }
        let pending = self.retry_pending.clone();
        let timer = cx.background_executor().timer(delay);
        let task = cx.spawn(async move |_| {
            timer.await;
            pending.set(false);
        });
        self.refresh_when(task, cx);
    }

    fn submit_entity(cx: &App) -> Option<gpui::Entity<Submit>> {
        Submit::try_global(cx)
    }

    /// Notes as `@` rows. They come before project files everywhere.
    fn note_files(&self, cx: &mut App) -> Vec<ProjectFile> {
        self.with_notes(cx, monocode_engine::history::notes::notes_as_project_files)
            .into_iter()
            .map(|note| ProjectFile {
                name: note.name,
                path: note.path,
                relative: note.relative,
                is_dir: false,
            })
            .collect()
    }

    /// Notes ranked for an `@` query.
    fn ranked_notes(&self, query: &str, cx: &mut App) -> Vec<RankedFile> {
        self.with_notes(cx, |notes| {
            monocode_engine::history::notes::rank_note_files(notes, query)
        })
        .into_iter()
        .map(|note| RankedFile {
            file: ProjectFile {
                name: note.name,
                path: note.path,
                relative: note.relative,
                is_dir: false,
            },
            score: note.score,
            positions: note.positions,
        })
        .collect()
    }

    /// The project listing, or `None` while it loads. A project not listed
    /// yet starts its scan and refreshes the composer when it lands.
    fn listing(&self, cwd: &str, cx: &mut App) -> Option<Listing> {
        if self.listing_cwd.borrow().as_deref() != Some(cwd) {
            *self.listing_cwd.borrow_mut() = Some(cwd.to_string());
        }
        let files = Files::try_global(cx).map(|files| files.index.clone())?;
        if let Some(listed) = files.read(cx).peek_project_files(cwd) {
            // A listing that may be out of date re-lists in the background;
            // a change arrives as `ProjectFilesChanged`.
            files.update(cx, |index, cx| index.revalidate(cwd, cx));
            return Some(listed);
        }
        let load = files.update(cx, |index, cx| index.load_project_files(cwd, false, cx));
        // A folder that is not a project, or one whose scan just failed,
        // answers at once without a listing. Refreshing on that answer
        // would ask again forever.
        if let Some(answer) = load.clone().now_or_never() {
            // A failed scan answers until its retry delay ends; ask again
            // then. A folder that is not a project never lists.
            if answer.is_err() {
                self.refresh_after(FAILED_SCAN_RETRY, cx);
            }
            return None;
        }
        let task = cx.background_spawn(async move {
            let _ = load.await;
        });
        self.refresh_when(task, cx);
        None
    }

    /// Runs `read` over the cached notes, without copying them. Notes not
    /// loaded yet start loading and refresh the composer when done.
    fn with_notes<R>(
        &self,
        cx: &mut App,
        read: impl FnOnce(&[monocode_engine::history::notes::Note]) -> R,
    ) -> R {
        if !monocode_settings::settings_store::load_notes_enabled(&self.kv) {
            return read(&[]);
        }
        let Some(package) = HistoryPackage::try_global(cx) else {
            return read(&[]);
        };
        let notes = package.notes.clone();
        if let Some(cached) = notes.read(cx).peek_notes() {
            return read(cached);
        }
        let load = notes.update(cx, |notes, cx| notes.load_notes(false, cx));
        let task = cx.background_spawn(async move {
            load.await;
        });
        self.refresh_when(task, cx);
        read(&[])
    }
}

impl ComposerHost for SessionComposerHost {
    fn submit(&self, submission: ComposerSubmission, window: &mut Window, cx: &mut App) -> bool {
        // A workspace switch is moving this session. Reject before async
        // preparation can make the composer clear its draft.
        if monocode_app::bridge::ActiveWorkspace::get(cx)
            .and_then(|workspace| workspace.upgrade())
            .is_some_and(|workspace| workspace.read(cx).is_switching(&self.session_id, cx))
        {
            return false;
        }
        let Some(submit) = Self::submit_entity(cx) else {
            return false;
        };
        let window = window.window_handle();
        let on_resend_rejected = submission.resend.and_then(|ticket| {
            self.composer.borrow().clone().map(|composer| {
                Rc::new(move |rejection, cx: &mut App| {
                    let composer = composer.clone();
                    cx.defer(move |cx| {
                        let _ = window.update(cx, |_, window, cx| {
                            let _ = composer.update(cx, |composer, cx| {
                                composer.resend_rejected(ticket, rejection, window, cx)
                            });
                        });
                    });
                }) as monocode_engine::submit::pipeline::OnResendRejected
            })
        });
        let options = SubmitOptions {
            intent: submission.options.intent,
            resend_edited: submission.options.resend_edited == Some(true),
            draft_block_id: submission.options.draft_block_id,
            on_resend_rejected,
            ..SubmitOptions::default()
        };
        let id = self.session_id.clone();
        submit.update(cx, |submit, cx| {
            submit.on_submit(&id, &submission.text, submission.attachments, options, cx)
        })
    }

    fn stop(&self, _: &mut Window, cx: &mut App) {
        if let Some(submit) = Self::submit_entity(cx) {
            let id = self.session_id.clone();
            submit.update(cx, |submit, cx| submit.stop(&id, false, cx));
        }
    }

    fn save_draft(
        &self,
        text: String,
        attachments: Vec<Attachment>,
        _: &mut Window,
        cx: &mut App,
    ) -> bool {
        let Some(submit) = Self::submit_entity(cx) else {
            return false;
        };
        let id = self.session_id.clone();
        submit.update(cx, |submit, cx| {
            submit.save_draft(&id, &text, attachments, None, cx)
        })
    }

    /// Open the side sheet, with a draft or a submitted question.
    fn btw(&self, text: String, draft: bool, _: &mut Window, cx: &mut App) -> bool {
        self.btw_sheet
            .borrow()
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .is_some_and(|sheet| sheet.update(cx, |sheet, cx| sheet.open_with(&text, draft, cx)))
    }

    fn compact_context(&self, _: &mut Window, cx: &mut App) -> bool {
        let Some(submit) = Self::submit_entity(cx) else {
            return false;
        };
        let id = self.session_id.clone();
        submit.update(cx, |submit, cx| submit.compact(&id, cx))
    }

    fn draft_changed(&self, text: &str, cx: &mut App) {
        if let Some(submit) = Self::submit_entity(cx) {
            submit
                .read(cx)
                .drafts()
                .set_composer_draft(&self.session_id, text);
        }
    }

    fn place_in_folder(&self, target: FolderTarget, _: &mut Window, cx: &mut App) {
        let sessions = Engine::sessions(cx);
        let Some(cwd) = sessions
            .read(cx)
            .get(&self.session_id)
            .map(|session| session.cwd.clone())
        else {
            return;
        };
        let target = match target {
            FolderTarget::Existing { folder_id } => {
                monocode_engine::history::session_folders::SessionFolderTarget::Existing {
                    folder_id,
                }
            }
            FolderTarget::New { name } => {
                monocode_engine::history::session_folders::SessionFolderTarget::New { name }
            }
        };
        monocode_engine::history::sidebar::place_session_in_project_folder(
            &self.kv,
            &cwd,
            &self.session_id,
            &target,
        );
    }

    fn session_folders(&self, cwd: &str, _: &mut App) -> Vec<SessionFolder> {
        monocode_engine::history::session_folders::load_session_folders(&self.kv, cwd)
            .into_iter()
            .map(|folder| SessionFolder {
                id: folder.id,
                name: folder.name,
                session_count: folder.session_ids.len(),
            })
            .collect()
    }

    fn load_mcp_tags(&self, session_id: &str, cx: &mut App) -> Vec<McpTag> {
        Submit::try_global(cx)
            .map(|submit| {
                submit
                    .read(cx)
                    .drafts()
                    .get_composer_mcp_tags(session_id)
                    .into_iter()
                    .map(|tag| McpTag {
                        server: mcp_connection(&tag.server),
                        token: tag.token,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_mcp_tags(&self, session_id: &str, tags: &[McpTag], cx: &mut App) {
        if let Some(submit) = Submit::try_global(cx) {
            let tags = tags
                .iter()
                .filter_map(|tag| {
                    Some(monocode_engine::submit::mcp_picker::McpTag {
                        server: engine_mcp_connection(&tag.server)?,
                        token: tag.token.clone(),
                    })
                })
                .collect();
            submit
                .read(cx)
                .drafts()
                .set_composer_mcp_tags(session_id, tags);
        }
    }

    fn raw_slash_commands(&self, harness: HarnessId) -> bool {
        self.registry
            .as_ref()
            .and_then(|registry| registry.get_harness(harness))
            .and_then(|provider| provider.commands())
            .is_some_and(|commands| commands.raw_slash_commands())
    }

    fn create_skill(
        &self,
        cwd: &str,
        name: &str,
        scope: NewSkillScope,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Some(catalog) = self.skills.clone() else {
            return Task::ready(Err("The skill catalog is unavailable.".into()));
        };
        let (cwd, name) = (cwd.to_owned(), name.to_owned());
        let scope = match scope {
            NewSkillScope::Project => engine_skills::FileSkillScope::Project,
            NewSkillScope::User => engine_skills::FileSkillScope::User,
        };
        cx.background_spawn(async move { catalog.create_blank_skill(&cwd, &name, scope).await })
    }

    fn attachments_from_paths(&self, paths: Vec<String>, cx: &mut App) -> Task<Vec<Attachment>> {
        let Some(io) = self.attachment_io.clone() else {
            return Task::ready(Vec::new());
        };
        cx.background_spawn(async move {
            attachments::attachments_from_paths(io.as_ref(), &paths)
                .await
                .unwrap_or_default()
        })
    }

    fn attachments_from_files(
        &self,
        files: Vec<ClipboardFile>,
        cx: &mut App,
    ) -> Task<Vec<Attachment>> {
        let Some(io) = self.attachment_io.clone() else {
            return Task::ready(Vec::new());
        };
        let pasted: Vec<PastedFile> = files
            .into_iter()
            .map(|file| PastedFile {
                name: file.name,
                mime_type: file.mime_type,
                bytes: file.bytes,
            })
            .collect();
        cx.background_spawn(async move {
            attachments::attachments_from_files(io.as_ref(), &[], &pasted)
                .await
                .unwrap_or_default()
        })
    }

    /// `pickAttachments`: the open panel, files and folders.
    fn pick_attachments(&self, _: &mut Window, cx: &mut App) -> Task<Vec<Attachment>> {
        let Some(io) = self.attachment_io.clone() else {
            return Task::ready(Vec::new());
        };
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: None,
        });
        cx.background_spawn(async move {
            let paths: Vec<PathBuf> = match picked.await {
                Ok(Ok(Some(paths))) => paths,
                _ => return Vec::new(),
            };
            let paths: Vec<String> = paths
                .into_iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect();
            attachments::attachments_from_paths(io.as_ref(), &paths)
                .await
                .unwrap_or_default()
        })
    }

    /// `useComposerSkills().skills`: the cached catalog. A catalog that is
    /// not loaded yet starts loading and refreshes the composer when done.
    fn skills(&self, context: &SkillContext, cx: &mut App) -> Vec<Skill> {
        let Some(catalog) = &self.skills else {
            return Vec::new();
        };
        let context = catalog_context(context, cx);
        if let Some(skills) = catalog.peek_skills(&context) {
            return skills.iter().map(picker_skill).collect();
        }
        let load = catalog.load_skills(&context, false);
        // A native catalog inside its retry window answers at once and
        // stays out of the cache. Refreshing on that answer would ask
        // again, answer again, and spin the UI thread until the window
        // ends.
        if let Some(skills) = load.clone().now_or_never() {
            // Ask once more when the window ends, so a failed load recovers.
            self.refresh_after(
                Duration::from_millis(engine_skills::NATIVE_SKILL_RETRY_MS as u64),
                cx,
            );
            return skills.iter().map(picker_skill).collect();
        }
        let task = cx.background_spawn(async move {
            load.await;
        });
        self.refresh_when(task, cx);
        Vec::new()
    }

    fn reload_skills(&self, context: &SkillContext, refresh: bool, cx: &mut App) {
        let Some(catalog) = &self.skills else {
            return;
        };
        let load = catalog.load_skills(&catalog_context(context, cx), refresh);
        let task = cx.background_spawn(async move {
            load.await;
        });
        self.refresh_when(task, cx);
    }

    fn has_native_commands(&self, harness: HarnessId) -> bool {
        self.skills
            .as_ref()
            .is_some_and(|catalog| catalog.has_native_commands(harness))
    }

    /// The project index for `@`. A project not listed yet starts its scan
    /// and refreshes the composer when it lands.
    fn mention_files(&self, cwd: &str, cx: &mut App) -> Vec<ProjectFile> {
        let notes = self.note_files(cx);
        let listing = self.listing(cwd, cx);
        mention_rows(listing.as_ref(), notes)
    }

    /// The `@` label index, built once per listing and shared by every
    /// composer in the project. A large listing builds on the background
    /// executor; until it lands the composer keeps the previous index.
    fn mention_index(&self, cwd: &str, cx: &mut App) -> Arc<MentionIndex> {
        let notes = self.note_files(cx);
        let listing = self.listing(cwd, cx);
        let indexes = cx.default_global::<MentionIndexes>();
        indexes.clock += 1;
        let clock = indexes.clock;
        let previous = match indexes.built.get_mut(cwd) {
            Some(built) if built.inputs.matches(listing.as_ref(), &notes) => {
                built.used = clock;
                return built.index.clone();
            }
            Some(built) => built.index.clone(),
            None => Arc::default(),
        };
        let size = listing.as_ref().map_or(0, |listing| listing.len()) + notes.len();
        if size <= INLINE_LISTING {
            let index = Arc::new(build_mention_index(&mention_rows(
                listing.as_ref(),
                notes.clone(),
            )));
            indexes.store(
                cwd.to_string(),
                MentionInputs::new(listing.as_ref(), notes),
                index.clone(),
            );
            return index;
        }
        let running = indexes
            .building
            .get(cwd)
            .filter(|build| build.inputs.matches(listing.as_ref(), &notes))
            .map(|build| build.task.clone());
        let task = match running {
            Some(task) => task,
            None => {
                let id = clock;
                let key = cwd.to_string();
                let inputs = MentionInputs::new(listing.as_ref(), notes.clone());
                let build =
                    cx.background_spawn({
                        let listing = listing.clone();
                        async move {
                            Arc::new(build_mention_index(&mention_rows(listing.as_ref(), notes)))
                        }
                    });
                let task = cx
                    .spawn(async move |cx| {
                        let index = build.await;
                        cx.update(|cx| {
                            let indexes = cx.default_global::<MentionIndexes>();
                            // Only the newest build for the project lands.
                            if let Some(build) = indexes.building.remove(&key) {
                                if build.id == id {
                                    indexes.store(key, build.inputs, index.clone());
                                } else {
                                    indexes.building.insert(key, build);
                                }
                            }
                        });
                        index
                    })
                    .shared();
                cx.default_global::<MentionIndexes>().building.insert(
                    cwd.to_string(),
                    MentionIndexBuild {
                        id,
                        inputs,
                        task: task.clone(),
                    },
                );
                task
            }
        };
        self.refresh_when(
            cx.spawn(async move |_| {
                task.await;
            }),
            cx,
        );
        previous
    }

    fn rank_mentions(&self, cwd: &str, query: &str, cx: &mut App) -> Vec<RankedFile> {
        let mut result = self.ranked_notes(query, cx);
        let Some(files) = Files::try_global(cx).map(|files| files.index.clone()) else {
            return result;
        };
        files.update(cx, |index, cx| index.revalidate(cwd, cx));
        let index = files.read(cx);
        let Some(listed) = index.peek_project_files(cwd) else {
            return result;
        };
        let recents = index.recent_opened_files(cwd);
        result.extend(ranked_file_rows(&listed, query, &recents));
        result
    }

    fn rank_mentions_task(
        &self,
        cwd: &str,
        query: &str,
        cx: &mut App,
    ) -> Option<Task<Vec<RankedFile>>> {
        let files = Files::try_global(cx).map(|files| files.index.clone())?;
        files.update(cx, |index, cx| index.revalidate(cwd, cx));
        let (listed, recents) = {
            let index = files.read(cx);
            let listed = index.peek_project_files(cwd)?;
            (listed, index.recent_opened_files(cwd))
        };
        if listed.len() <= INLINE_LISTING {
            return None;
        }
        let notes = self.ranked_notes(query, cx);
        let query = query.to_string();
        Some(cx.background_spawn(async move {
            let mut result = notes;
            result.extend(ranked_file_rows(&listed, &query, &recents));
            result
        }))
    }

    fn mentions_loading(&self, cwd: &str, cx: &mut App) -> bool {
        Files::try_global(cx)
            .is_some_and(|files| files.index.read(cx).peek_project_files(cwd).is_none())
    }

    fn model_source(&self, cx: &mut App) -> Option<Rc<dyn ModelSource>> {
        // The picker reads the catalog of the session's execution directory,
        // which is its worktree when it has one.
        let project = Engine::sessions(cx)
            .read(cx)
            .get(&self.session_id)
            .map(|session| monocode_core::session::session_work_cwd(session).to_string());
        match (AppServices::try_global(cx), project) {
            (Some(services), Some(project)) => Some(Rc::new(
                CatalogModelSource::from_services(services).for_project(Some(project)),
            )),
            _ => self.model_source.clone(),
        }
    }

    fn model_prefs(&self, _: &mut App) -> ModelPrefs {
        ModelPrefs::from_local_storage(|key| self.kv.get_item(key))
    }

    fn project_providers(&self, _: &mut App) -> ProjectProviders {
        ProjectProviders::parse(self.kv.get_item(PROJECT_PROVIDER_SETTINGS_KEY).as_deref())
    }

    fn mcp_servers(&self, cwd: &str, harness: HarnessId, cx: &mut App) -> McpServers {
        let Some(cache) = &self.mcp_cache else {
            return McpServers::default();
        };
        // Claude's servers depend on the session's profile.
        let account = session_account(&self.session_id, cx);
        let key =
            monocode_engine::submit::mcp_settings_cache::mcp_scope_key(cwd, account.as_deref());
        let cwd = key.as_str();
        if !self.mcp_watches.borrow().contains_key(cwd)
            && let Some(composer) = self.composer.borrow().clone()
        {
            let (tx, rx) = async_channel::bounded(1);
            let id = cache.subscribe_mcp_settings(cwd, move |_| {
                let _ = tx.try_send(());
            });
            let task = cx.spawn(async move |cx| {
                while rx.recv().await.is_ok() {
                    if composer
                        .update(cx, |composer, cx| composer.refresh_suggestions(cx))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            self.mcp_watches.borrow_mut().insert(
                cwd.to_owned(),
                McpWatch {
                    cache: cache.clone(),
                    cwd: cwd.to_owned(),
                    id,
                    claude_health: false,
                    _task: task,
                },
            );
        }
        let snapshot = cache.get_cached_mcp_settings(cwd);
        let request_health = harness == HarnessId::Claude
            && self
                .mcp_watches
                .borrow_mut()
                .get_mut(cwd)
                .is_some_and(|watch| {
                    if watch.claude_health {
                        return false;
                    }
                    watch.claude_health = true;
                    true
                });
        if snapshot.is_none() || request_health {
            let load = cache.load_mcp_settings(cwd, false, harness == HarnessId::Claude);
            let task = cx.background_spawn(async move {
                load.await;
            });
            self.refresh_when(task, cx);
        }
        let loading = snapshot.is_none();
        let snapshot = snapshot.unwrap_or_default();
        McpServers {
            servers: snapshot
                .servers
                .iter()
                .map(|row| mcp_connection(&row.connection))
                .collect(),
            claude_status: snapshot
                .servers
                .iter()
                .filter(|row| {
                    row.connection.provider == monocode_engine::submit::mcp::McpProvider::Claude
                })
                .map(|row| (row.connection.name.clone(), row.status.clone()))
                .collect(),
            loading,
            error: snapshot.error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use monocode_engine::workspace::files::backend::{FileMtime, FsEntry, FsFuture};
    use monocode_engine::workspace::files::{FsBackend, ProjectFile as ListedFile};
    use parking_lot::Mutex;

    const CWD: &str = "/Users/me/project";

    #[derive(Default)]
    struct Listing {
        files: Mutex<Vec<ListedFile>>,
        calls: Mutex<usize>,
    }

    impl FsBackend for Listing {
        fn list_project_files(&self, _: String) -> FsFuture<Vec<ListedFile>> {
            *self.calls.lock() += 1;
            let files = self.files.lock().clone();
            Box::pin(async move { Ok(files) })
        }

        fn stat_files(&self, _: Vec<String>) -> FsFuture<Vec<FileMtime>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn list_dir(&self, _: String) -> FsFuture<Vec<FsEntry>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    fn files(count: usize) -> Vec<ListedFile> {
        (0..count)
            .map(|n| {
                let relative = format!("src/file{n}.rs");
                ListedFile::new(format!("file{n}.rs"), format!("{CWD}/{relative}"), relative)
            })
            .collect()
    }

    fn setup(count: usize, cx: &mut TestAppContext) -> Arc<Listing> {
        let listing = Arc::new(Listing::default());
        *listing.files.lock() = files(count);
        cx.update(|cx| Files::init(listing.clone(), cx));
        listing
    }

    fn index(host: &SessionComposerHost, cx: &mut TestAppContext) -> Arc<MentionIndex> {
        cx.update(|cx| host.mention_index(CWD, cx))
    }

    #[gpui::test]
    fn composers_in_one_project_share_one_index(cx: &mut TestAppContext) {
        let listing = setup(3, cx);
        let first = cx.update(|cx| SessionComposerHost::new("a".into(), cx));
        let second = cx.update(|cx| SessionComposerHost::new("b".into(), cx));
        // Not listed yet: the scan starts and the index is empty for now.
        assert!(index(&first, cx).labels.is_empty());
        cx.run_until_parked();
        let built = index(&first, cx);
        assert!(built.labels.contains_key("file0.rs"));
        assert!(Arc::ptr_eq(&built, &index(&second, cx)));
        assert!(Arc::ptr_eq(&built, &index(&first, cx)));
        assert_eq!(*listing.calls.lock(), 1);
        assert!(first.reads_listing_of(CWD));
        assert!(!first.reads_listing_of("/Users/me/other"));
    }

    #[gpui::test]
    fn a_large_listing_builds_its_index_off_the_ui_thread(cx: &mut TestAppContext) {
        setup(INLINE_LISTING + 1, cx);
        let host = cx.update(|cx| SessionComposerHost::new("a".into(), cx));
        index(&host, cx);
        cx.run_until_parked();
        // The listing landed; the first read starts the build and keeps the
        // previous (empty) index until it lands.
        assert!(index(&host, cx).labels.is_empty());
        cx.run_until_parked();
        let built = index(&host, cx);
        assert!(built.labels.contains_key("file0.rs"));
        assert!(Arc::ptr_eq(&built, &index(&host, cx)));
    }

    #[gpui::test]
    fn a_new_listing_rebuilds_the_index(cx: &mut TestAppContext) {
        let listing = setup(2, cx);
        let host = cx.update(|cx| SessionComposerHost::new("a".into(), cx));
        index(&host, cx);
        cx.run_until_parked();
        let before = index(&host, cx);
        assert!(!before.labels.contains_key("file2.rs"));
        *listing.files.lock() = files(3);
        let index_entity = cx.update(|cx| Files::global(cx).index.clone());
        let load = index_entity.update(cx, |index, cx| index.load_project_files(CWD, true, cx));
        cx.run_until_parked();
        drop(load);
        let after = index(&host, cx);
        assert!(after.labels.contains_key("file2.rs"));
    }
}
