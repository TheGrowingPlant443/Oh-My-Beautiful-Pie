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
pub fn create_architect(
    app: AppHandle,
    state: State<'_, AppState>,
    config: CreateInput,
) -> Result<Instance> {
    let instance = state.registry.create_architect(config)?;
    let result = state.registry.start(&instance.id, Origin::Human);
    publish(&app, &state.registry);
    result?;
    state.registry.inspect(&instance.id, Origin::Human)
}
#[tauri::command]
pub async fn scan_dependencies(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::dependencies::DependencyGraph> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    blocking(move || {
        let mut graph = crate::dependencies::scan(Path::new(&folder), &registry.home, true)?;
        // The Architect reads the complete graph from an immutable snapshot. A save failure only costs that snapshot:
        // the graph still draws, and starting a new analysis reports the specific error again.
        if let Err(error) = context.publish(&registry, &insights, &inhouse, &graph).and_then(|summary| context.activate(&summary)) {
            graph.notes.push(format!("The dependency context for The Architect could not be saved: {error}"));
        }
        Ok(graph)
    }).await
}
#[tauri::command]
pub async fn get_dependency_classification(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::architect::Classification> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::architect::classification_for(&registry, Path::new(&folder))).await
}
#[tauri::command]
pub async fn classify_dependencies(app: AppHandle, window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::architect::ClassifyAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || {
        let result = crate::architect::classify(&registry, Path::new(&folder));
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn get_package_insights_status(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<crate::package_insights::PackageInsightsStatus> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let insights = state.package_insights.clone();
    blocking(move || Ok(insights.status())).await
}
#[tauri::command]
pub async fn recheck_github_credential(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<crate::package_insights::PackageInsightsStatus> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let insights = state.package_insights.clone();
    blocking(move || Ok(insights.recheck())).await
}
#[tauri::command]
pub async fn get_inhouse_status(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String, rescan: bool, gate: bool) -> Result<crate::inhouse::InhouseStatus> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let inhouse = state.inhouse.clone();
    blocking(move || inhouse.status(&registry, Path::new(&folder), rescan, gate)).await
}
#[tauri::command]
pub async fn start_inhouse_sweep(app: AppHandle, window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::architect::InhouseAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || { let result = crate::architect::inhouse_sweep(&registry, Path::new(&folder)); publish(&app, &registry); result }).await
}
#[tauri::command]
pub async fn get_project_kind(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::architect::ProjectKind> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::architect::project_kind_for(&registry, Path::new(&folder))).await
}
#[tauri::command]
pub async fn start_project_kind(app: AppHandle, window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::architect::KindAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || { let result = crate::architect::ask_project_kind(&registry, Path::new(&folder)); publish(&app, &registry); result }).await
}
#[tauri::command]
pub async fn get_known_standard(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<crate::known_standard::KnownStandard> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || Ok(crate::known_standard::read(registry.config_root()))).await
}
#[tauri::command]
pub async fn clear_known_standard(window: tauri::WebviewWindow, state: State<'_, AppState>, language: Option<String>) -> Result<usize> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::known_standard::clear(registry.config_root(), language.as_deref())).await
}
#[tauri::command]
pub async fn get_derived_standard(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<crate::derived_standard::DerivedStandard> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::derived_standard::resolve(Path::new(&folder), &registry.home, registry.config_root())).await
}
#[tauri::command]
pub async fn get_package_insights(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    package_id: String,
    github: crate::package_insights::GithubMode,
    refresh: bool,
) -> Result<crate::package_insights::PackageInsights> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let insights = state.package_insights.clone();
    let inhouse = state.inhouse.clone();
    blocking(move || { inhouse.allow(Path::new(&folder), &package_id)?; insights.lookup(&package_id, github, refresh) }).await
}
#[tauri::command]
pub async fn start_dependency_security_review(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    package_id: String,
    rerun: bool,
) -> Result<crate::dependency_security::DependencyJobAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let security = state.dependency_security.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    blocking(move || {
        let result = security.start(&registry, &insights, &inhouse, &context, Path::new(&folder), &package_id, rerun, crate::dependency_security::DependencyJobKind::Security);
        publish(&app, &registry);
        result
    }).await
}
#[tauri::command]
pub async fn read_dependency_security_review(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_security::SecurityReviewState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let security = state.dependency_security.clone();
    blocking(move || security.read(&registry, &request_id)).await
}
#[tauri::command]
pub async fn start_dependency_usage_explanation(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    package_id: String,
    rerun: bool,
) -> Result<crate::dependency_security::DependencyJobAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let security = state.dependency_security.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    blocking(move || {
        let result = security.start(&registry, &insights, &inhouse, &context, Path::new(&folder), &package_id, rerun, crate::dependency_security::DependencyJobKind::Usage);
        publish(&app, &registry);
        result
    }).await
}
#[tauri::command]
pub async fn read_dependency_usage_explanation(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_security::UsageExplanationState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let security = state.dependency_security.clone();
    blocking(move || security.read_usage(&registry, &request_id)).await
}
#[tauri::command]
pub async fn show_dependency_result_in_architect(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let security = state.dependency_security.clone();
    let project = state.dependency_project_review.clone();
    blocking(move || {
        if project.owns(&request_id) { project.show_in_architect(&registry, &security, &request_id) }
        else { security.show_in_architect(&registry, &request_id) }
    }).await
}
#[tauri::command]
pub async fn start_project_dependency_security_review(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    rerun: bool,
) -> Result<crate::dependency_security::DependencyJobAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let security = state.dependency_security.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    let project = state.dependency_project_review.clone();
    blocking(move || {
        let result = project.start(&registry, &security, &insights, &inhouse, &context, Path::new(&folder), rerun);
        publish(&app, &registry);
        result
    }).await
}
#[tauri::command]
pub async fn read_project_dependency_security_review(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_project_review::ProjectReviewState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let security = state.dependency_security.clone();
    let project = state.dependency_project_review.clone();
    blocking(move || project.read(&registry, &security, &request_id)).await
}
#[tauri::command]
pub async fn start_dependency_update_plan(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    review_request_id: String,
    regenerate: bool,
) -> Result<crate::dependency_security::DependencyJobAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    let security = state.dependency_security.clone();
    let project = state.dependency_project_review.clone();
    let updates = state.dependency_updates.clone();
    blocking(move || {
        let result = crate::dependency_updates::load_review_source(&registry, &security, &project, &review_request_id).and_then(|source| {
            let folder = Path::new(&folder);
            // Publishing the current scan runs only when a fresh draft is created; reuse never rescans.
            updates.start(&registry, folder, source, regenerate, || {
                let graph = crate::dependencies::scan(folder, &registry.home, true)?;
                let summary = context.publish(&registry, &insights, &inhouse, &graph)?;
                Ok(crate::dependency_updates::PlanningContext { id: summary.id, captured_at: summary.captured_at })
            })
        });
        publish(&app, &registry);
        result
    }).await
}
#[tauri::command]
pub async fn read_dependency_update_plan(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_updates::UpdatePlanState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let updates = state.dependency_updates.clone();
    blocking(move || updates.read(&registry, &request_id)).await
}
#[tauri::command]
pub async fn prepare_dependency_update_handoff(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
    revision_id: String,
) -> Result<crate::dependency_updates::HandoffPrepared> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let updates = state.dependency_updates.clone();
    blocking(move || updates.prepare_handoff(&registry, &request_id, &revision_id)).await
}
#[tauri::command]
pub async fn confirm_dependency_update_handoff(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    confirmation_token: String,
) -> Result<crate::dependency_updates::UpdatePlanHandoff> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    let updates = state.dependency_updates.clone();
    blocking(move || {
        let result = updates.confirm_handoff(&registry, &confirmation_token);
        // Starting The Manager changes the instance list even when the handoff itself ends unsent.
        publish(&app, &registry);
        result
    }).await
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
pub fn start_instance(app: AppHandle, state: State<'_, AppState>, id: String, fresh: Option<bool>) -> Result<()> {
    let result = state.registry.start_with(&id, Origin::Human, fresh.unwrap_or(false));
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
pub async fn list_all_terminals(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<Vec<crate::terminal::TerminalOwner>> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.owners()).await
}
#[tauri::command]
pub async fn list_terminal_history(window: tauri::WebviewWindow, state: State<'_, AppState>, id: Option<String>, cursor: Option<String>) -> Result<crate::terminal::HistoryPage> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.history(id.as_deref(), cursor.as_deref())).await
}
#[tauri::command]
pub async fn read_terminal_script(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, session_id: String, execution_id: String) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.read_script(&id, &session_id, &execution_id)).await
}
#[tauri::command]
pub async fn read_terminal_output(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, session_id: String, execution_id: String) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.read_output(&id, &session_id, &execution_id)).await
}
#[tauri::command]
pub fn attach_command_output(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, generation: u64, session_id: String, execution_id: String, on_data: Channel<crate::terminal::CommandOutput>) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    state.terminals.attach_output(&id, generation, &session_id, &execution_id, on_data)
}
#[tauri::command]
pub async fn kill_terminal_command(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, generation: u64, session_id: String, execution_id: String) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.kill(&id, generation, &session_id, &execution_id)).await
}
#[tauri::command]
pub async fn open_user_terminal(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String) -> Result<crate::terminal::UserShellInfo> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.open_user_shell(&id)).await
}
#[tauri::command]
pub fn attach_user_terminal(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, shell_id: String, on_data: Channel<InvokeResponseBody>) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    state.terminals.attach_user_shell(&id, &shell_id, on_data)
}
#[tauri::command]
pub fn write_user_terminal(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, shell_id: String, data: String) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    state.terminals.write_user_shell(&id, &shell_id, &data)
}
#[tauri::command]
pub fn resize_user_terminal(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, shell_id: String, cols: u16, rows: u16) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    state.terminals.resize_user_shell(&id, &shell_id, cols, rows)
}
#[tauri::command]
pub async fn close_user_terminal(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, shell_id: String) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let terminals = state.terminals.clone();
    blocking(move || terminals.close_user_shell(&id, &shell_id)).await
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
#[tauri::command]
pub async fn open_helper(
    app: AppHandle,
    state: State<'_, AppState>,
    target_id: String,
    purpose: crate::instance::HelperPurpose,
    request: Option<crate::helper_agent::HelperRequest>,
    mode: Option<crate::code_diff::DiffMode>,
) -> Result<crate::helper_agent::HelperAdmission> {
    let registry = state.registry.clone();
    let helpers = state.helpers.clone();
    blocking(move || {
        let result = helpers.open(&registry, &target_id, purpose, request, mode);
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
#[tauri::command]
pub async fn explain_git_change(window: tauri::WebviewWindow, state: State<'_, AppState>, id: String, kind: String, base: Option<String>, target: String) -> Result<String> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let registry = state.registry.clone();
    blocking(move || crate::explain::explain(&registry, &id, &kind, base.as_deref(), &target)).await
}
use crate::code_quality::types::{
    QualityConfig, QualityCredentialUpdate, QualityProject, QualityProjectDetection, QualityRuntime, QualityScan, QualityScanPreparation,
    QualityState, ReportPreview, ScanAdmission,
};
#[tauri::command]
pub fn get_code_quality_state(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    Ok(state.code_quality.state())
}
#[tauri::command]
pub async fn check_code_quality_prerequisites(window: tauri::WebviewWindow, state: State<'_, AppState>) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.check_prerequisites()).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn configure_code_quality(window: tauri::WebviewWindow, state: State<'_, AppState>, config: QualityConfig, expectedRevision: u64, credential: Option<QualityCredentialUpdate>) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.configure(config, expectedRevision, credential)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn install_code_quality_runtime(window: tauri::WebviewWindow, state: State<'_, AppState>, runtime: QualityRuntime, expectedRevision: u64, consent: bool) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.install_runtime(runtime, expectedRevision, consent)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn enable_code_quality_windows_features(window: tauri::WebviewWindow, state: State<'_, AppState>, expectedRevision: u64, consent: bool) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.enable_windows_features(expectedRevision, consent)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn start_code_quality_service(window: tauri::WebviewWindow, state: State<'_, AppState>, expectedRevision: u64) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.start_service(expectedRevision)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn acknowledge_code_quality_rules(window: tauri::WebviewWindow, state: State<'_, AppState>, manifestHash: String, licenseHash: String, expectedRevision: u64, consent: bool) -> Result<QualityState> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.acknowledge_rules(manifestHash, licenseHash, expectedRevision, consent)).await
}
#[tauri::command]
pub async fn detect_code_quality_project(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<QualityProjectDetection> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.detect_project(&folder)).await
}
#[tauri::command]
pub async fn prepare_code_quality_scan(window: tauri::WebviewWindow, state: State<'_, AppState>, project: QualityProject) -> Result<QualityScanPreparation> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.prepare_scan(project)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn start_code_quality_scan(window: tauri::WebviewWindow, state: State<'_, AppState>, project: QualityProject, recipeHash: String, consent: bool) -> Result<ScanAdmission> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.start_scan(project, recipeHash, consent)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn read_code_quality_scan(window: tauri::WebviewWindow, state: State<'_, AppState>, scanId: String) -> Result<QualityScan> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.read_scan(&scanId)).await
}
#[tauri::command]
pub async fn list_code_quality_scans(window: tauri::WebviewWindow, state: State<'_, AppState>, folder: String) -> Result<Vec<QualityScan>> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.list_scans(&folder)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn cancel_code_quality_scan(window: tauri::WebviewWindow, state: State<'_, AppState>, scanId: String) -> Result<QualityScan> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.cancel_scan(&scanId)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn read_code_quality_report(window: tauri::WebviewWindow, state: State<'_, AppState>, scanId: String, reportName: String) -> Result<ReportPreview> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.read_report(&scanId, &reportName)).await
}
#[tauri::command]
#[allow(non_snake_case)]
pub async fn open_code_quality_dashboard(window: tauri::WebviewWindow, state: State<'_, AppState>, scanId: Option<String>) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.open_dashboard(scanId)).await
}
#[tauri::command]
pub async fn open_code_quality_sso_login(window: tauri::WebviewWindow, state: State<'_, AppState>, url: String) -> Result<()> {
    if window.label() != "main" { return Err("unauthorized window".into()); }
    let service = state.code_quality.clone();
    blocking(move || service.open_sso_login(&url)).await
}
pub fn tick(app: AppHandle, registry: Arc<Registry>, terminals: Arc<crate::terminal::TerminalService>, closing: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !closing.load(std::sync::atomic::Ordering::Relaxed) {
            if let Err(e) = registry.refresh() {
                eprintln!("session refresh: {e}");
            }
            let rows = registry.snapshot();
            if let Some(hub) = registry.manager_hub() {
                hub.observe_processes(&rows);
            }
            {
                // Local-only reconciliation of unfinished project security reviews; AppState is managed shortly after tick starts.
                use tauri::Manager as _;
                if let Some(state) = app.try_state::<AppState>() {
                    state.dependency_project_review.reconcile(&registry, &state.dependency_security);
                }
            }
            let _ = app.emit("instances", rows);
            if let Some(browser) = registry.browser_supervisor() {
                if let Some(update) = browser.take_update() { let _ = app.emit("browser_state", update); }
            }
            terminals.poll_shells();
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}
