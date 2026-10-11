#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
mod instance;
mod manager;
mod pty;
mod session;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Manager;

pub struct AppState {
    pub registry: Arc<instance::Registry>,
    pub closing: Arc<AtomicBool>,
}

fn main() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{
            SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        };
        if SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS) == 0 {
            panic!(
                "could not secure DLL search path: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let registry = Arc::new(
                instance::Registry::load(
                    app.path().app_config_dir()?.join("instances.json"),
                    app.path().home_dir()?,
                )
                .map_err(std::io::Error::other)?,
            );
            let folder = app.path().app_config_dir()?;
            std::fs::create_dir_all(&folder)?;
            let extension = folder.join("omp-manager-bridge.mjs");
            let source = include_str!("../../bridge/omp-manager.mjs");
            if std::fs::read_to_string(&extension).ok().as_deref() != Some(source) {
                std::fs::write(&extension, source)?;
            }
            let hub = manager::ManagerHub::bind(extension).map_err(std::io::Error::other)?;
            hub.bind_registry(Arc::downgrade(&registry))
                .map_err(std::io::Error::other)?;
            registry
                .install_manager_hub(hub)
                .map_err(std::io::Error::other)?;
            registry.refresh().map_err(std::io::Error::other)?;
            let closing = Arc::new(AtomicBool::new(false));
            commands::tick(app.handle().clone(), registry.clone(), closing.clone());
            app.manage(AppState { registry, closing });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_instances,
            commands::create_instance,
            commands::create_manager,
            commands::set_manager_owner,
            commands::return_to_manager,
            commands::start_instance,
            commands::stop_instance,
            commands::restart_instance,
            commands::remove_instance,
            commands::attach_pty,
            commands::write_pty,
            commands::resize_pty,
            commands::list_sessions,
            commands::read_plan,
            commands::render_transcript,
        ])
        .build(tauri::generate_context!())
        .expect("could not initialize OMP Manager");
    app.run(|handle, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            let state = handle.state::<AppState>();
            if !state.closing.swap(true, Ordering::Relaxed) {
                api.prevent_exit();
                let registry = state.registry.clone();
                let handle = handle.clone();
                std::thread::spawn(move || {
                    registry.shutdown();
                    handle.exit(0);
                });
            }
        }
    });
}
