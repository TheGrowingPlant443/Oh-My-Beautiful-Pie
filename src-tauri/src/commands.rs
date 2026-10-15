use crate::instance::{CreateInput, Instance, Origin, Registry};
use crate::{session, AppState};
use std::path::Path;
use std::sync::Arc;
use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};

pub type Result<T, E = String> = std::result::Result<T, E>;
fn publish(app: &AppHandle, registry: &Registry) {
    let _ = app.emit("instances", registry.snapshot());
}
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
pub fn list_instances(state: State<'_, AppState>) -> Vec<Instance> {
    state.registry.snapshot()
}
#[tauri::command]
pub async fn configure_browser(window: tauri::WebviewWindow, state: State<'_, AppState>, settings: crate::browser::BrowserSettings) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let browser = state.browser.clone();
    blocking(move || browser.configure(settings)).await
}
#[tauri::command]
pub fn list_browsers(state: State<'_, AppState>) -> Vec<crate::browser::BrowserInfo> {
    state.browser.snapshot()
}
#[tauri::command]
pub async fn open_session_browser(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    generation: u64,
    restart_headless: bool,
) -> Result<crate::browser::BrowserInfo> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, browser) = (state.registry.clone(), state.browser.clone());
    blocking(move || {
        if registry.lease(&id, Origin::Human, false)?.generation != generation {
            return Err("stale_generation".into());
        }
        browser.open_visible(&id, generation, restart_headless)
    })
    .await
}
#[tauri::command]
pub fn set_browser_view(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    instance_id: Option<String>,
    generation: Option<u64>,
    rect: Option<crate::browser::BrowserRect>,
) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    #[cfg(windows)]
    {
        let view = match (instance_id, generation, rect) {
            (None, None, None) => None,
            (Some(instance_id), Some(generation), Some(rect)) => {
                if ![rect.x, rect.y, rect.width, rect.height].iter().all(|value| value.is_finite() && *value >= 0.0 && *value < 100_000.0) {
                    return Err("invalid browser bounds".into());
                }
                if state.registry.lease(&instance_id, Origin::Human, false)?.generation != generation {
                    return Err("stale_generation".into());
                }
                let scale = window.scale_factor().map_err(|e| e.to_string())?;
                Some(crate::browser_window::View {
                    instance_id,
                    generation,
                    rect: crate::browser::BrowserRect { x: rect.x * scale, y: rect.y * scale, width: rect.width * scale, height: rect.height * scale },
                })
            }
            _ => return Err("browser view requires an instance, generation and bounds".into()),
        };
        state.browser_host.set_view(view);
    }
    #[cfg(not(windows))]
    let _ = (&state, instance_id, generation, rect);
    Ok(())
}
#[tauri::command]
pub fn create_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    config: CreateInput,
) -> Result<Instance> {
    let instance = state.registry.create(config)?;
    publish(&app, &state.registry);
    Ok(instance)
}
#[tauri::command]
pub fn create_manager(
    app: AppHandle,
    state: State<'_, AppState>,
    config: CreateInput,
) -> Result<Instance> {
    let instance = state.registry.create_manager(config)?;
    let result = state.registry.start(&instance.id, Origin::Human);
    publish(&app, &state.registry);
    result?;
    state.registry.inspect(&instance.id, Origin::Human)
}
#[tauri::command]
pub fn set_manager_owner(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    manager_id: Option<String>,
) -> Result<()> {
    state
        .registry
        .set_manager_owner(&id, manager_id.as_deref())?;
    publish(&app, &state.registry);
    Ok(())
}
#[tauri::command]
pub fn return_to_manager(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    state.registry.return_to_manager(&id)?;
    publish(&app, &state.registry);
    Ok(())
}
#[tauri::command]
pub fn start_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    let result = state.registry.start(&id, Origin::Human);
    publish(&app, &state.registry);
    result
}
#[tauri::command]
pub async fn stop_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry.stop(&id, Origin::Human)?;
        publish(&app, &registry);
        Ok(())
    })
    .await
}
#[tauri::command]
pub async fn restart_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry.stop(&id, Origin::Human)?;
        registry.refresh()?;
        let result = registry.start(&id, Origin::Human);
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn remove_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry.remove(&id, Origin::Human)?;
        publish(&app, &registry);
        Ok(())
    })
    .await
}
#[tauri::command]
pub fn attach_pty(
    state: State<'_, AppState>,
    id: String,
    on_data: Channel<InvokeResponseBody>,
) -> Result<()> {
    state.registry.attach(&id, on_data)
}
#[tauri::command]
pub fn write_pty(state: State<'_, AppState>, id: String, data: String) -> Result<()> {
    state.registry.write(&id, &data, Origin::Human)
}
#[tauri::command]
pub fn resize_pty(state: State<'_, AppState>, id: String, cols: u16, rows: u16) -> Result<()> {
    state.registry.resize(&id, cols, rows, Origin::Human)
}
#[tauri::command]
pub fn list_sessions(
    state: State<'_, AppState>,
    project_path: String,
    profile: String,
) -> Vec<session::SessionInfo> {
    session::list(&state.registry.home, Path::new(&project_path), &profile)
}
#[tauri::command]
pub fn read_plan(state: State<'_, AppState>, id: String) -> Result<Option<String>> {
    state.registry.plan(&id, Origin::Human)
}
#[tauri::command]
pub fn list_plan_backups(state: State<'_, AppState>, id: String) -> Result<Vec<crate::plan::PlanBackup>> {
    state.registry.inspect(&id, Origin::Human)?;
    crate::plan::list_backups(&state.registry.plan_folder(&id))
}
#[tauri::command]
pub async fn restore_plan_backup(app: AppHandle, state: State<'_, AppState>, id: String, backup_id: String) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry.manager_hub().ok_or("manager bridge not initialized")?.restore_plan_backup(&id, &backup_id)?;
        publish(&app, &registry);
        Ok(())
    }).await
}
#[tauri::command]
pub async fn render_transcript(
    state: State<'_, AppState>,
    id: String,
    subagent_id: String,
) -> Result<String> {
    let (path, input) = state
        .registry
        .transcript_path(&id, &subagent_id, Origin::Human)?;
    blocking(move || session::render_transcript_file(&path, &input)).await
}
#[tauri::command]
pub fn get_global_config(state: State<'_, AppState>) -> crate::config::GlobalConfigInfo {
    state.registry.global_config.info()
}
#[tauri::command]
pub async fn preview_global_config(
    state: State<'_, AppState>,
    source_id: Option<String>,
    import_path: Option<String>,
) -> Result<crate::config::ConfigPreview> {
    if source_id.is_some() && import_path.is_some() {
        return Err("Select a source instance or import file, not both".into());
    }
    let registry = state.registry.clone();
    let request = if let Some(path) = import_path {
        serde_json::json!({"action":"import","path":path,"cwd":registry.home,"profile":"default"})
    } else if let Some(id) = source_id {
        let source = registry.inspect(&id, Origin::Human)?;
        serde_json::json!({"action":"export","cwd":source.project_path,"profile":source.profile,"configFiles":source.configs})
    } else {
        serde_json::json!({"action":"export","cwd":registry.home,"profile":"default"})
    };
    blocking(move || crate::config::preview(registry.config_root(), &registry.home, request)).await
}
#[tauri::command]
pub async fn save_global_config(
    state: State<'_, AppState>,
    yaml: String,
) -> Result<serde_json::Value> {
    let registry = state.registry.clone();
    blocking(move || {
        let clean = crate::config::preview(registry.config_root(), &registry.home,
            serde_json::json!({"action":"import","yaml":yaml,"cwd":registry.home,"profile":"default"}))?;
        let path = registry.global_config.publish(&clean.yaml)?;
        Ok(serde_json::json!({"path":path}))
    }).await
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedConfigs {
    pub current: Option<std::path::PathBuf>,
    pub snapshots: Vec<crate::config::SavedConfig>,
    pub native_global: Option<std::path::PathBuf>,
    pub error: Option<String>,
}
#[tauri::command]
pub async fn list_saved_configs(state: State<'_, AppState>) -> Result<SavedConfigs> {
    let registry = state.registry.clone();
    blocking(move || {
        let info = registry.global_config.info();
        let mut error = info.error;
        let native_global = match crate::setup::paths(&registry) {
            Ok(paths) => Some(paths.global_agent_folder.join("config.yml")).filter(|p| p.is_file()),
            Err(message) => {
                if error.is_none() {
                    error = Some(message);
                }
                None
            }
        };
        Ok(SavedConfigs {
            current: info.path,
            snapshots: registry.global_config.list(),
            native_global,
            error,
        })
    })
    .await
}
#[tauri::command]
pub async fn activate_global_config(
    state: State<'_, AppState>,
    path: String,
) -> Result<serde_json::Value> {
    let registry = state.registry.clone();
    blocking(move || {
        let path = registry.global_config.activate(Path::new(&path))?;
        Ok(serde_json::json!({"path":path}))
    })
    .await
}
#[tauri::command]
pub async fn get_setup_paths(state: State<'_, AppState>) -> Result<crate::setup::SetupPaths> {
    let registry = state.registry.clone();
    blocking(move || crate::setup::paths(&registry)).await
}
#[tauri::command]
pub async fn get_repository_setup_root(
    state: State<'_, AppState>,
    project_instance_id: String,
) -> Result<std::path::PathBuf> {
    let registry = state.registry.clone();
    blocking(move || crate::setup::repository_root(&registry, &project_instance_id)).await
}
#[tauri::command]
pub async fn list_agent_files(state: State<'_, AppState>) -> Result<crate::setup::AgentFiles> {
    let registry = state.registry.clone();
    blocking(move || Ok(crate::setup::agent_files(&registry))).await
}
#[tauri::command]
pub async fn list_omp_profiles(state: State<'_, AppState>) -> Result<crate::setup::OmpProfiles> {
    let registry = state.registry.clone();
    blocking(move || Ok(crate::setup::omp_profiles(&registry))).await
}
#[tauri::command]
pub async fn open_setup_folder(
    state: State<'_, AppState>,
    scope: crate::setup::SetupScope,
    project_instance_id: Option<String>,
) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || crate::setup::open_folder(&registry, scope, project_instance_id.as_deref())).await
}
#[tauri::command]
pub async fn start_agent_setup(
    app: AppHandle,
    state: State<'_, AppState>,
    request: crate::setup::SetupRequest,
) -> Result<crate::setup::SetupAdmission> {
    let registry = state.registry.clone();
    let setup = state.setup.clone();
    blocking(move || {
        let paths = crate::setup::paths(&registry)?;
        let result = setup.start(&registry, &paths, request);
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn send_agent_setup(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    request: crate::setup::SetupRequest,
) -> Result<crate::setup::SetupAdmission> {
    let registry = state.registry.clone();
    let setup = state.setup.clone();
    blocking(move || {
        let result = setup.send(&registry, &id, &request);
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn open_plan_session(
    app: AppHandle,
    state: State<'_, AppState>,
    target_id: String,
) -> Result<crate::plan::PlanAdmission> {
    let registry = state.registry.clone();
    let plan = state.plan.clone();
    blocking(move || {
        let result = plan.open(&registry, &target_id);
        publish(&app, &registry);
        result
    })
    .await
}
fn diff_project(registry: &Registry, id: &str) -> Result<String> {
    Ok(registry.inspect(id, Origin::Human)?.project_path)
}
#[tauri::command]
pub async fn get_code_diff(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, mode: crate::code_diff::DiffMode) -> Result<crate::code_diff::DiffOverview> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.overview(&id, Path::new(&diff_project(&registry, &id)?), mode)).await
}
#[tauri::command]
pub async fn read_code_diff_file(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, snapshot_id: String, path: String, group: Option<crate::code_diff::DiffGroup>) -> Result<crate::code_diff::DiffContent> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.read_file(&id, Path::new(&diff_project(&registry, &id)?), &snapshot_id, &path, group)).await
}
#[tauri::command]
pub async fn save_code_diff_file(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    snapshot_id: String,
    path: String,
    group: Option<crate::code_diff::DiffGroup>,
    expected_hash: Option<String>,
    text: String,
) -> Result<crate::code_diff::DiffContent> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.save_file(&id, Path::new(&diff_project(&registry, &id)?), &snapshot_id, &path, group, expected_hash.as_deref(), &text)).await
}
#[tauri::command]
pub async fn change_code_diff_file(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    snapshot_id: String,
    path: String,
    group: Option<crate::code_diff::DiffGroup>,
    action: crate::code_diff::FileAction,
) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.change_file(&id, Path::new(&diff_project(&registry, &id)?), &snapshot_id, &path, group, action)).await
}
#[tauri::command]
pub async fn commit_code_diff(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, snapshot_id: String, message: String) -> Result<crate::code_diff::CommitResult> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.commit(&id, Path::new(&diff_project(&registry, &id)?), &snapshot_id, &message)).await
}
#[tauri::command]
pub async fn publish_code_diff(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, snapshot_id: String, options: crate::code_diff::PublishOptions) -> Result<crate::code_diff::PublishResult> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.publish(&id, Path::new(&diff_project(&registry, &id)?), &snapshot_id, &options)).await
}
#[tauri::command]
pub async fn open_code_diff_pull_request(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let diff = state.code_diff.clone();
    blocking(move || diff.open_pull_request(&id)).await
}
#[tauri::command]
pub async fn get_git_graph(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, limit: u32) -> Result<crate::git_graph::GitGraph> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::graph(Path::new(&diff_project(&registry, &id)?), limit)).await
}
#[tauri::command]
pub async fn get_commit_diff(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, base: Option<String>, target: String) -> Result<crate::code_diff::DiffOverview> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.commit_overview(&id, Path::new(&diff_project(&registry, &id)?), base.as_deref(), &target)).await
}
#[tauri::command]
pub async fn revert_commit(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, commit: String) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::revert(Path::new(&diff_project(&registry, &id)?), &commit)).await
}
#[tauri::command]
pub async fn cherry_pick_commit(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, commit: String) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::cherry_pick(Path::new(&diff_project(&registry, &id)?), &commit)).await
}
#[tauri::command]
pub async fn reset_last_commit(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::reset_last(Path::new(&diff_project(&registry, &id)?))).await
}
#[tauri::command]
pub async fn create_branch_at(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, name: String, commit: Option<String>) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::create_branch(Path::new(&diff_project(&registry, &id)?), &name, commit.as_deref())).await
}
pub fn tick(app: AppHandle, registry: Arc<Registry>, closing: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !closing.load(std::sync::atomic::Ordering::Relaxed) {
            if let Err(e) = registry.refresh() {
                eprintln!("session refresh: {e}");
            }
            let rows = registry.snapshot();
            if let Some(hub) = registry.manager_hub() {
                hub.observe_processes(&rows);
            }
            let _ = app.emit("instances", rows);
            if let Some(browser) = registry.browser_supervisor() {
                if let Some(update) = browser.take_update() { let _ = app.emit("browser_state", update); }
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}
