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
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}
