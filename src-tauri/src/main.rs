#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod browser;
#[cfg(windows)]
mod browser_window;
mod code_diff;
mod commands;
mod config;
mod git_graph;
mod instance;
mod manager;
mod pty;
mod plan;
mod session;
mod setup;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Manager;

pub struct AppState {
    pub registry: Arc<instance::Registry>,
    pub closing: Arc<AtomicBool>,
    pub setup: Arc<setup::SetupService>,
    pub plan: Arc<plan::PlanService>,
    pub browser: Arc<browser::BrowserSupervisor>,
    pub code_diff: Arc<code_diff::DiffService>,
    #[cfg(windows)]
    pub browser_host: Arc<browser_window::BrowserHost>,
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
            config::materialize(&folder).map_err(std::io::Error::other)?;
            setup::materialize(&folder).map_err(std::io::Error::other)?;
            let browser = browser::BrowserSupervisor::new(folder.clone(), app.path().home_dir()?);
            browser.materialize().map_err(std::io::Error::other)?;
            #[cfg(windows)]
            let browser_host = Arc::new(browser_window::BrowserHost::default());
            #[cfg(windows)]
            browser_host.start(app.handle().clone(), browser.clone());
            registry.install_browser_supervisor(browser.clone()).map_err(std::io::Error::other)?;
            let extension = folder.join("omp-manager-bridge.mjs");
            let source = include_str!("../../bridge/omp-manager.mjs");
            if std::fs::read_to_string(&extension).ok().as_deref() != Some(source) {
                std::fs::write(&extension, source)?;
            }
            let hub = manager::ManagerHub::bind(extension).map_err(std::io::Error::other)?;
            hub.bind_registry(Arc::downgrade(&registry))
                .map_err(std::io::Error::other)?;
            hub.install_browser_supervisor(browser.clone()).map_err(std::io::Error::other)?;
            registry
                .install_manager_hub(hub)
                .map_err(std::io::Error::other)?;
            registry.refresh().map_err(std::io::Error::other)?;
            let closing = Arc::new(AtomicBool::new(false));
            commands::tick(app.handle().clone(), registry.clone(), closing.clone());
            app.manage(AppState {
                registry,
                closing,
                browser: browser.clone(),
                code_diff: Arc::new(code_diff::DiffService::new()),
                #[cfg(windows)]
                browser_host: browser_host.clone(),
                setup: Arc::new(setup::SetupService::new(folder)),
                plan: Arc::new(plan::PlanService::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_instances,
            commands::configure_browser,
            commands::list_browsers,
            commands::open_session_browser,
            commands::set_browser_view,
            commands::get_code_diff,
            commands::read_code_diff_file,
            commands::save_code_diff_file,
            commands::change_code_diff_file,
            commands::commit_code_diff,
            commands::publish_code_diff,
            commands::open_code_diff_pull_request,
            commands::get_git_graph,
            commands::get_commit_diff,
            commands::revert_commit,
            commands::cherry_pick_commit,
            commands::reset_last_commit,
            commands::create_branch_at,
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
            commands::list_plan_backups,
            commands::restore_plan_backup,
            commands::render_transcript,
            commands::get_global_config,
            commands::preview_global_config,
            commands::save_global_config,
            commands::list_saved_configs,
            commands::activate_global_config,
            commands::list_agent_files,
            commands::list_omp_profiles,
            commands::get_setup_paths,
            commands::get_repository_setup_root,
            commands::start_agent_setup,
            commands::send_agent_setup,
            commands::open_setup_folder,
            commands::open_plan_session,
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
