#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent_markdown;
mod architect;
mod browser;
#[cfg(windows)]
mod browser_window;
mod code_diff;
mod code_quality;
mod commands;
mod config;
mod dependencies;
mod dependency_context;
mod dependency_project_review;
mod dependency_security;
mod dependency_updates;
mod derived_standard;
mod inhouse;
mod known_standard;
mod explain;
mod git_graph;
mod helper_agent;
mod instance;
mod manager;
mod pty;
mod plan;
mod package_insights;
mod project_insight;
mod project_requirements;
mod session;
mod setup;
mod host_monitor;
mod knowledge;
mod srs;
mod source_index;
mod system_overview;
mod terminal;
mod toolchain;
mod workspace;
mod workspace_native;
#[cfg(windows)]
mod process_tree;
#[cfg(unix)]
#[path = "process_tree_unix.rs"]
mod process_tree;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Manager;

pub struct AppState {
    pub registry: Arc<instance::Registry>,
    pub workspace: Arc<workspace::WorkspaceService>,
    pub closing: Arc<AtomicBool>,
    pub setup: Arc<setup::SetupService>,
    pub plan: Arc<plan::PlanService>,
    pub helpers: Arc<helper_agent::HelperService>,
    pub browser: Arc<browser::BrowserSupervisor>,
    pub code_diff: Arc<code_diff::DiffService>,
    pub code_quality: Arc<code_quality::CodeQualityService>,
    pub terminals: Arc<terminal::TerminalService>,
    pub package_insights: Arc<package_insights::PackageInsightsService>,
    pub dependency_security: Arc<dependency_security::DependencySecurityService>,
    pub dependency_context: Arc<dependency_context::DependencyContextService>,
    pub dependency_project_review: Arc<dependency_project_review::DependencyProjectReviewService>,
    pub dependency_updates: Arc<dependency_updates::DependencyUpdateService>,
    pub project_insight: Arc<project_insight::ProjectInsightService>,
    pub knowledge: Arc<knowledge::KnowledgeService>,
    pub source_index: Arc<source_index::SourceIndexService>,
    pub agent_markdown: Arc<agent_markdown::AgentMarkdownService>,
    pub srs: Arc<srs::SrsService>,
    pub host_monitor: Arc<host_monitor::HostMonitorService>,
    pub inhouse: Arc<inhouse::InhouseService>,
    #[cfg(windows)]
    pub browser_host: Arc<browser_window::BrowserHost>,
}

fn main() {
    // A desktop-launched app inherits a PATH that can lack `~/.local/bin`, `~/.nix-profile/bin` or Homebrew.
    #[cfg(unix)]
    toolchain::extend_path_for_desktop_launch();
    #[cfg(unix)]
    if let Some(exit_code) = code_quality::cli::dispatch() {
        std::process::exit(exit_code);
    }
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
            let terminals = terminal::TerminalService::new(&folder);
            terminal::TerminalService::materialize(&folder).map_err(std::io::Error::other)?;
            helper_agent::materialize(&folder).map_err(std::io::Error::other)?;
            code_quality::materialize_bridge(&folder).map_err(std::io::Error::other)?;
            architect::materialize(&folder).map_err(std::io::Error::other)?;
            workspace_native::materialize(&folder).map_err(std::io::Error::other)?;
            terminals.bind_app(app.handle().clone()).map_err(std::io::Error::other)?;
            let hub = manager::ManagerHub::bind(extension).map_err(std::io::Error::other)?;
            hub.bind_app(app.handle().clone()).map_err(std::io::Error::other)?;
            hub.bind_registry(Arc::downgrade(&registry))
                .map_err(std::io::Error::other)?;
            terminals.bind_registry(Arc::downgrade(&registry)).map_err(std::io::Error::other)?;
            hub.install_browser_supervisor(browser.clone()).map_err(std::io::Error::other)?;
            hub.install_terminal_service(terminals.clone()).map_err(std::io::Error::other)?;
            registry.install_terminal_service(terminals.clone()).map_err(std::io::Error::other)?;
            registry
                .install_manager_hub(hub.clone())
                .map_err(std::io::Error::other)?;
            let workspace = workspace::WorkspaceService::new(registry.clone(), hub.clone(), folder.clone());
            workspace.bind_app(app.handle().clone()).map_err(std::io::Error::other)?;
            hub.install_workspace_service(Arc::downgrade(&workspace)).map_err(std::io::Error::other)?;
            let dependency_updates = Arc::new(dependency_updates::DependencyUpdateService::new(folder.clone()));
            dependency_updates.bind_workspace(workspace.clone()).map_err(std::io::Error::other)?;
            workspace.bind_dependency_updates(Arc::downgrade(&dependency_updates)).map_err(std::io::Error::other)?;
            registry.refresh().map_err(std::io::Error::other)?;
            if let Err(error) = workspace.reconcile_computed_folders() { eprintln!("Reconciling computed workspace folders: {error}"); }
            let closing = Arc::new(AtomicBool::new(false));
            commands::tick(app.handle().clone(), registry.clone(), terminals.clone(), closing.clone());
            let package_insights = Arc::new(
                package_insights::PackageInsightsService::new(folder.clone())
                    .map_err(std::io::Error::other)?,
            );
            let dependency_security = Arc::new(
                dependency_security::DependencySecurityService::new(folder.clone()),
            );
            let inhouse = Arc::new(inhouse::InhouseService::new(folder.clone()));
            let code_quality = code_quality::CodeQualityService::new(folder.clone());
            code_quality.reconcile_on_start();
            hub.install_code_quality_service(Arc::downgrade(&code_quality)).map_err(std::io::Error::other)?;
            let source_index = source_index::SourceIndexService::new(folder.clone());
            let project_insight = Arc::new(project_insight::ProjectInsightService::new(folder.clone()));
            project_insight.bind_source_facts(source_index.clone());
            let knowledge = knowledge::KnowledgeService::new(folder.clone(), registry.home.clone());
            let setup_service = Arc::new(setup::SetupService::new(folder.clone()));
            let agent_markdown = agent_markdown::AgentMarkdownService::new(registry.config_root().to_path_buf(), knowledge.clone());
            setup_service.install_markdown(agent_markdown.clone()).map_err(std::io::Error::other)?;
            let srs = srs::SrsService::new(folder.clone(), knowledge.clone());
            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                srs.set_notifier(Box::new(move |status| { let _ = handle.emit("srs_job", status); })).map_err(std::io::Error::other)?;
            }
            srs.bind_registry(Arc::downgrade(&registry)).map_err(std::io::Error::other)?;
            hub.install_srs_service(Arc::downgrade(&srs)).map_err(std::io::Error::other)?;
            let host_monitor = host_monitor::HostMonitorService::new(registry.clone());
            hub.install_host_monitor_service(Arc::downgrade(&host_monitor)).map_err(std::io::Error::other)?;
            agent_markdown.install_boundary(Arc::new(manager::HubMarkdownBoundary(Arc::downgrade(&registry)))).map_err(std::io::Error::other)?;
            app.manage(AppState {
                registry,
                workspace,
                closing,
                knowledge,
                agent_markdown,
                srs,
                host_monitor,
                browser: browser.clone(),
                code_diff: Arc::new(code_diff::DiffService::new()),
                code_quality,
                terminals,
                package_insights,
                dependency_security,
                dependency_context: Arc::new(dependency_context::DependencyContextService::new(folder.clone())),
                dependency_project_review: Arc::new(dependency_project_review::DependencyProjectReviewService::new(folder.clone())),
                dependency_updates,
                project_insight,
                source_index,
                inhouse,
                #[cfg(windows)]
                browser_host: browser_host.clone(),
                setup: setup_service,
                plan: Arc::new(plan::PlanService::new()),
                helpers: Arc::new(helper_agent::HelperService::new()),
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
            commands::get_code_quality_state,
            commands::check_code_quality_prerequisites,
            commands::configure_code_quality,
            commands::install_code_quality_runtime,
            commands::enable_code_quality_windows_features,
            commands::start_code_quality_service,
            commands::setup_code_quality,
            commands::enter_code_quality,
            commands::take_over_code_quality,
            commands::shutdown_code_quality_server,
            commands::reveal_code_quality_credentials,
            commands::set_code_quality_start_at_launch,
            commands::save_code_quality_report,
            commands::acknowledge_code_quality_rules,
            commands::detect_code_quality_project,
            commands::prepare_code_quality_scan,
            commands::start_code_quality_scan,
            commands::read_code_quality_scan,
            commands::list_code_quality_scans,
            commands::list_code_quality_project_folders,
            commands::cancel_code_quality_scan,
            commands::read_code_quality_report,
            commands::open_code_quality_dashboard,
            commands::open_code_quality_sso_login,
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
            commands::ensure_coordinators,
            commands::create_manager,
            commands::create_architect,
            commands::scan_dependencies,
            commands::get_project_requirements,
            commands::get_dependency_classification,
            commands::classify_dependencies,
            commands::get_package_insights,
            commands::get_package_insights_status,
            commands::recheck_github_credential,
            commands::get_inhouse_status,
            commands::start_inhouse_sweep,
            commands::get_project_kind,
            commands::start_project_kind,
            commands::get_known_standard,
            commands::clear_known_standard,
            commands::get_derived_standard,
            commands::start_dependency_security_review,
            commands::read_dependency_security_review,
            commands::start_dependency_usage_explanation,
            commands::read_dependency_usage_explanation,
            commands::show_dependency_result_in_architect,
            commands::dependency_manifest_status,
            commands::request_project_insight,
            commands::get_project_insight,
            commands::request_system_visualization,
            commands::read_project_insight,
            commands::approve_structure_preview,
            commands::list_structure_sources,
            commands::read_structure_source,
            commands::select_last_structure_source,
            commands::get_source_index,
            commands::start_source_index,
            commands::read_source_index_job,
            commands::cancel_source_index,
            commands::query_source_index,
            commands::prepare_manifest_creation,
            commands::confirm_manifest_creation,
            commands::start_project_dependency_security_review,
            commands::read_project_dependency_security_review,
            commands::start_dependency_update_plan,
            commands::read_dependency_update_plan,
            commands::prepare_dependency_update_handoff,
            commands::confirm_dependency_update_handoff,
            commands::change_agent_directory,
            commands::list_computed_workspace_folders,
            commands::hand_over_to_manager,
            commands::take_back_control,
            commands::give_repo_to_architect,
            commands::list_workspace_changes,
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
            commands::list_all_terminals,
            commands::list_terminal_history,
            commands::read_terminal_script,
            commands::explain_git_change,
            commands::open_helper,
            commands::read_terminal_output,
            commands::attach_command_output,
            commands::open_native_agent_hub,
            commands::list_human_decisions,
            commands::respond_human_decision,
            commands::kill_terminal_command,
            commands::open_user_terminal,
            commands::attach_user_terminal,
            commands::write_user_terminal,
            commands::resize_user_terminal,
            commands::get_knowledge_capabilities,
            commands::list_knowledge_vaults,
            commands::prepare_knowledge_vault,
            commands::confirm_knowledge_vault,
            commands::open_knowledge_note,
            commands::open_obsidian_vault_manager,
            commands::open_knowledge_link,
            commands::open_obsidian_download,
            commands::record_obsidian_decline,
            commands::configure_obsidian_executable,
            commands::list_vault_entries,
            commands::read_vault_document,
            commands::query_vault_links,
            commands::refresh_vault_index,
            commands::open_graphify_artifact,
            commands::register_graphify_output,
            commands::read_graphify_graph,
            commands::query_graphify_graph,
            commands::open_cognee_ui,
            commands::open_host_monitor,
            commands::collect_host_monitor,
            commands::read_host_monitor,
            commands::close_host_monitor,
            commands::explain_host_monitor,
            commands::explain_host_monitor_after_srs_pause,
            commands::cancel_host_monitor_explanation,
            commands::get_agent_markdown_status,
            commands::prepare_agent_markdown_import,
            commands::confirm_agent_markdown_import,
            commands::prepare_agent_markdown_apply,
            commands::check_graphify_artifact,
            commands::prepare_srs_scope,
            commands::start_srs,
            commands::get_srs_job,
            commands::list_srs_jobs,
            commands::pause_srs,
            commands::cancel_srs,
            commands::prepare_srs_resume,
            commands::resume_srs,
            commands::dispose_srs_item,
            commands::query_srs_job,
            commands::open_srs_documents,
            commands::rebind_srs_vault,
            commands::apply_agent_markdown,
            commands::close_user_terminal,
        ])
        .build(tauri::generate_context!())
        .expect("could not initialize OMP Manager");
    app.run(|handle, event| {
        if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
            let state = handle.state::<AppState>();
            if !state.closing.swap(true, Ordering::Relaxed) {
                api.prevent_exit();
                let registry = state.registry.clone();
                let quality = state.code_quality.clone();
                let srs = state.srs.clone();
                let workspace = state.workspace.clone();
                let monitor = state.host_monitor.clone();
                let knowledge = state.knowledge.clone();
                let handle = handle.clone();
                std::thread::spawn(move || {
                    monitor.shutdown();
                    srs.shutdown();
                    workspace.shutdown();
                    quality.shutdown();
                    knowledge.shutdown();
                    registry.shutdown();
                    handle.exit(0);
                });
            } else if code.is_none() {
                // Repeated window closes must not bypass the in-flight cleanup.
                // The cleanup worker's explicit exit(0) is the terminal request.
                api.prevent_exit();
            }
        }
    });
}
