use std::path::Path;
use std::sync::Arc;

use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{AppHandle, Emitter, State};

use crate::instance::{CreateInput, Instance, Registry};
use crate::{session, AppState};

pub type Result<T, E = String> = std::result::Result<T, E>;

fn publish(app: &AppHandle, registry: &Registry) {
    let _ = app.emit("instances", registry.snapshot());
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
pub fn start_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    let result = state.registry.start(&id);
    publish(&app, &state.registry);
    result
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
pub async fn stop_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry.stop(&id)?;
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
        registry.stop(&id)?;
        registry.refresh()?;
        let result = registry.start(&id);
        publish(&app, &registry);
        result
    })
    .await
}

#[tauri::command]
pub async fn remove_instance(app: AppHandle, state: State<'_, AppState>, id: String) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry.remove(&id)?;
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
    state.registry.write(&id, &data)
}
#[tauri::command]
pub fn resize_pty(state: State<'_, AppState>, id: String, cols: u16, rows: u16) -> Result<()> {
    state.registry.resize(&id, cols, rows)
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
    state.registry.plan(&id)
}
#[tauri::command]
pub async fn render_transcript(
    state: State<'_, AppState>,
    id: String,
    subagent_id: String,
) -> Result<String> {
    let (path, input) = state.registry.transcript_path(&id, &subagent_id)?;
    blocking(move || {
        let program = std::env::var_os("OMP_BIN").unwrap_or_else(|| "omp".into());
        let mut command = std::process::Command::new(program);
        command
            .arg("render")
            .arg(path)
            .args(["-w", "120", "--plain"]);
        command.current_dir(&input.project_path);
        command.env("OMP_PROFILE", &input.profile);
        let mut overlays: Vec<std::path::PathBuf> = std::env::var_os("PI_CONFIG_FILES")
            .map(|s| std::env::split_paths(&s).collect())
            .unwrap_or_default();
        overlays.extend(input.configs);
        command.env(
            "PI_CONFIG_FILES",
            std::env::join_paths(overlays).map_err(|e| e.to_string())?,
        );
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = command.output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned());
        }
        // Native renderer owns formatting/thinking visibility. Never parse message contents here.
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
}

pub fn tick(app: AppHandle, registry: Arc<Registry>, closing: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !closing.load(std::sync::atomic::Ordering::Relaxed) {
            if let Err(e) = registry.refresh() {
                eprintln!("session refresh: {e}");
            }
            publish(&app, &registry);
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    });
}
