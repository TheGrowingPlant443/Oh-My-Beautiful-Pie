use crate::code_quality::types::{
    QualityConfig, QualityCredentialUpdate, QualityProject, QualityProjectDetection,
    QualityRuntime, QualityScan, QualityScanPreparation, QualityState, ReportPreview,
    ScanAdmission,
};
use crate::instance::{CreateInput, Instance, Origin, Registry};
use crate::knowledge;
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
pub async fn ensure_coordinators(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    preferred_folders: Vec<String>,
) -> Result<crate::instance::CoordinatorBootstrap> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        let result = registry.ensure_coordinators(&preferred_folders);
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn configure_browser(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    settings: crate::browser::BrowserSettings,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, browser) = (state.registry.clone(), state.browser.clone());
    blocking(move || {
        if registry
            .lease(&id, Origin::Human, /* mutation */ false)?
            .generation
            != generation
        {
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    #[cfg(windows)]
    {
        let view = match (instance_id, generation, rect) {
            (None, None, None) => None,
            (Some(instance_id), Some(generation), Some(rect)) => {
                if ![rect.x, rect.y, rect.width, rect.height]
                    .iter()
                    .all(|value| value.is_finite() && *value >= 0.0 && *value < 100_000.0)
                {
                    return Err("invalid browser bounds".into());
                }
                if state
                    .registry
                    .lease(&instance_id, Origin::Human, /* mutation */ false)?
                    .generation
                    != generation
                {
                    return Err("stale_generation".into());
                }
                let scale = window.scale_factor().map_err(|e| e.to_string())?;
                Some(crate::browser_window::View {
                    instance_id,
                    generation,
                    rect: crate::browser::BrowserRect {
                        x: rect.x * scale,
                        y: rect.y * scale,
                        width: rect.width * scale,
                        height: rect.height * scale,
                    },
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
pub async fn scan_dependencies(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::dependencies::DependencyGraph> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    blocking(move || {
        let mut graph = crate::dependencies::scan(Path::new(&folder), &registry.home, true)?;
        // The Architect reads the complete graph from an immutable snapshot. A save failure only costs that snapshot:
        // the graph still draws, and starting a new analysis reports the specific error again.
        if let Err(error) = context
            .publish(&registry, &insights, &inhouse, &graph)
            .and_then(|summary| context.activate(&summary))
        {
            graph.notes.push(format!(
                "The dependency context for The Architect could not be saved: {error}"
            ));
        }
        Ok(graph)
    })
    .await
}
#[tauri::command]
pub async fn get_project_requirements(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::project_requirements::ProjectRequirements> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    // Passive: tool versions plus the Code quality service's read-only probe seam; nothing is started or installed.
    let service = state.code_quality.clone();
    blocking(move || {
        crate::project_requirements::analyze(
            Path::new(&folder),
            &crate::project_requirements::SystemHost::new(service),
        )
    })
    .await
}
#[tauri::command]
pub async fn get_dependency_classification(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::architect::Classification> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::architect::classification_for(&registry, Path::new(&folder))).await
}
#[tauri::command]
pub async fn classify_dependencies(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::architect::ClassifyAdmission> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        let result = crate::architect::classify(&registry, Path::new(&folder));
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn get_package_insights_status(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<crate::package_insights::PackageInsightsStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let insights = state.package_insights.clone();
    blocking(move || Ok(insights.status())).await
}
#[tauri::command]
pub async fn recheck_github_credential(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<crate::package_insights::PackageInsightsStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let insights = state.package_insights.clone();
    blocking(move || Ok(insights.recheck())).await
}
#[tauri::command]
pub async fn get_inhouse_status(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    rescan: bool,
    gate: bool,
) -> Result<crate::inhouse::InhouseStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let inhouse = state.inhouse.clone();
    blocking(move || inhouse.status(&registry, Path::new(&folder), rescan, gate)).await
}
#[tauri::command]
pub async fn start_inhouse_sweep(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::architect::InhouseAdmission> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        let result = crate::architect::inhouse_sweep(&registry, Path::new(&folder));
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn get_project_kind(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::architect::ProjectKind> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::architect::project_kind_for(&registry, Path::new(&folder))).await
}
#[tauri::command]
pub async fn start_project_kind(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::architect::KindAdmission> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        let result = crate::architect::ask_project_kind(&registry, Path::new(&folder));
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn get_known_standard(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<crate::known_standard::KnownStandard> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || Ok(crate::known_standard::read(registry.config_root()))).await
}
#[tauri::command]
pub async fn clear_known_standard(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    language: Option<String>,
) -> Result<usize> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::known_standard::clear(registry.config_root(), language.as_deref()))
        .await
}
#[tauri::command]
pub async fn get_derived_standard(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::derived_standard::DerivedStandard> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        crate::derived_standard::resolve(Path::new(&folder), &registry.home, registry.config_root())
    })
    .await
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let insights = state.package_insights.clone();
    let inhouse = state.inhouse.clone();
    blocking(move || {
        inhouse.allow(Path::new(&folder), &package_id)?;
        insights.lookup(&package_id, github, refresh)
    })
    .await
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let security = state.dependency_security.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    blocking(move || {
        let result = security.start(
            &registry,
            &insights,
            &inhouse,
            &context,
            Path::new(&folder),
            &package_id,
            rerun,
            crate::dependency_security::DependencyJobKind::Security,
        );
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn read_dependency_security_review(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_security::SecurityReviewState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let security = state.dependency_security.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    blocking(move || {
        let result = security.start(
            &registry,
            &insights,
            &inhouse,
            &context,
            Path::new(&folder),
            &package_id,
            rerun,
            crate::dependency_security::DependencyJobKind::Usage,
        );
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn read_dependency_usage_explanation(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_security::UsageExplanationState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let security = state.dependency_security.clone();
    let project = state.dependency_project_review.clone();
    let insight = state.project_insight.clone();
    blocking(move || {
        if project.owns(&request_id) {
            project.show_in_architect(&registry, &security, &request_id)
        } else if insight.owns(&request_id) {
            insight.show_in_architect(&registry, &request_id)
        } else {
            security.show_in_architect(&registry, &request_id)
        }
    })
    .await
}
#[tauri::command]
pub async fn dependency_manifest_status(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::dependencies::ManifestStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::dependencies::manifest_status(Path::new(&folder), &registry.home)).await
}
#[tauri::command]
pub async fn request_project_insight(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    kind: crate::project_insight::InsightKind,
    rerun: Option<bool>,
) -> Result<crate::project_insight::ProjectInsightStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insight = state.project_insight.clone();
    blocking(move || {
        let result = insight.request(&registry, Path::new(&folder), kind, rerun.unwrap_or(false));
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn get_project_insight(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    kind: crate::project_insight::InsightKind,
) -> Result<Option<crate::project_insight::ProjectInsightStatus>> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insight = state.project_insight.clone();
    blocking(move || insight.get(&registry, Path::new(&folder), kind)).await
}
#[tauri::command]
pub async fn request_system_visualization(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    kind: crate::project_insight::InsightKind,
    snapshot_id: String,
    source_id: Option<String>,
    base_request_id: Option<String>,
    supersedes_request_id: Option<String>,
) -> Result<crate::project_insight::ProjectInsightStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insight = state.project_insight.clone();
    blocking(move || {
        let result = insight.request_visualization(
            &registry,
            Path::new(&folder),
            kind,
            &snapshot_id,
            source_id.as_deref(),
            base_request_id.as_deref(),
            supersedes_request_id.as_deref(),
        );
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn read_project_insight(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::project_insight::ProjectInsightStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insight = state.project_insight.clone();
    blocking(move || insight.status(&registry, &request_id)).await
}
#[tauri::command]
pub async fn approve_structure_preview(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
    expected_result_hash: String,
    expected_source_hash: String,
) -> Result<crate::project_insight::ProjectInsightStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insight = state.project_insight.clone();
    blocking(move || {
        insight.approve_structure_preview(
            &registry,
            &request_id,
            &expected_result_hash,
            &expected_source_hash,
        )
    })
    .await
}
#[tauri::command]
pub async fn list_structure_sources(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    architect_id: String,
    expected_generation: u64,
) -> Result<crate::system_overview::StructureSourceList> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        crate::system_overview::list_structure_sources(
            &registry,
            &architect_id,
            expected_generation,
        )
    })
    .await
}
#[tauri::command]
pub async fn read_structure_source(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    architect_id: String,
    expected_generation: u64,
    source_id: String,
    offset: u64,
) -> Result<crate::system_overview::StructureSourcePage> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        crate::system_overview::read_structure_source(
            &registry,
            &architect_id,
            expected_generation,
            &source_id,
            offset,
        )
    })
    .await
}
#[tauri::command]
pub async fn select_last_structure_source(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    architect_id: String,
    expected_generation: u64,
) -> Result<crate::system_overview::StructureSourceRow> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        crate::system_overview::select_last_structure_source(
            &registry,
            &architect_id,
            expected_generation,
        )
    })
    .await
}
#[tauri::command]
pub async fn prepare_manifest_creation(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
    ecosystem: String,
) -> Result<crate::project_insight::ManifestPreparation> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insight = state.project_insight.clone();
    blocking(move || insight.prepare_manifest(&registry, &request_id, &ecosystem)).await
}
#[tauri::command]
pub async fn confirm_manifest_creation(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    confirmation_token: String,
) -> Result<crate::project_insight::ManifestCreated> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let insight = state.project_insight.clone();
    blocking(move || insight.confirm_manifest(&confirmation_token)).await
}
#[tauri::command]
pub async fn start_project_dependency_security_review(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    rerun: bool,
) -> Result<crate::dependency_security::DependencyJobAdmission> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let security = state.dependency_security.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    let project = state.dependency_project_review.clone();
    blocking(move || {
        let result = project.start(
            &registry,
            &security,
            &insights,
            &inhouse,
            &context,
            Path::new(&folder),
            rerun,
        );
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn read_project_dependency_security_review(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_project_review::ProjectReviewState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let insights = state.package_insights.clone();
    let inhouse = state.inhouse.clone();
    let context = state.dependency_context.clone();
    let security = state.dependency_security.clone();
    let project = state.dependency_project_review.clone();
    let updates = state.dependency_updates.clone();
    blocking(move || {
        let result = crate::dependency_updates::load_review_source(
            &registry,
            &security,
            &project,
            &review_request_id,
        )
        .and_then(|source| {
            let folder = Path::new(&folder);
            // Publishing the current scan runs only when a fresh draft is created; reuse never rescans.
            updates.start(&registry, folder, source, regenerate, || {
                let graph = crate::dependencies::scan(folder, &registry.home, true)?;
                let summary = context.publish(&registry, &insights, &inhouse, &graph)?;
                Ok(crate::dependency_updates::PlanningContext {
                    id: summary.id,
                    captured_at: summary.captured_at,
                })
            })
        });
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn read_dependency_update_plan(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    request_id: String,
) -> Result<crate::dependency_updates::UpdatePlanState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    let updates = state.dependency_updates.clone();
    blocking(move || {
        let result = updates.confirm_handoff(&registry, &confirmation_token);
        // Starting The Manager changes the instance list even when the handoff itself ends unsent.
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub fn change_agent_directory(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    folder: String,
    expected_generation: u64,
) -> Result<crate::workspace::WorkspaceChange> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state
        .workspace
        .change_directory(id, folder, expected_generation)
}
#[tauri::command]
pub fn list_computed_workspace_folders(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<String>> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.workspace.computed_folders()
}
#[tauri::command]
pub fn hand_over_to_manager(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    source_id: String,
) -> Result<crate::workspace::WorkspaceChange> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.workspace.hand_over_to_manager(source_id)
}
#[tauri::command]
pub fn take_back_control(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.workspace.take_back_control(&id)
}
#[tauri::command]
pub fn return_to_manager(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    human_request_id: String,
) -> Result<crate::workspace::WorkspaceChange> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.workspace.return_to_manager(id, human_request_id)
}
#[tauri::command]
pub fn give_repo_to_architect(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    source_id: String,
) -> Result<crate::workspace::WorkspaceChange> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.workspace.give_repo_to_architect(source_id)
}
#[tauri::command]
pub fn list_workspace_changes(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<crate::workspace::WorkspaceChange>> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    Ok(state.workspace.list())
}
#[tauri::command]
pub fn start_instance(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    fresh: Option<bool>,
) -> Result<()> {
    let result = state
        .registry
        .start_with(&id, Origin::Human, fresh.unwrap_or(false));
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
pub async fn get_source_index(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::source_index::SourceIndexSelection> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.source_index.clone();
    blocking(move || service.get(&folder)).await
}
#[tauri::command]
pub async fn start_source_index(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<crate::source_index::SourceIndexStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.source_index.clone();
    blocking(move || service.start(&folder)).await
}
#[tauri::command]
pub async fn read_source_index_job(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<crate::source_index::SourceIndexStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.source_index.clone();
    blocking(move || service.read_job(&job_id)).await
}
#[tauri::command]
pub async fn cancel_source_index(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<crate::source_index::SourceIndexStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.source_index.clone();
    blocking(move || service.cancel(&job_id)).await
}
#[tauri::command]
pub async fn query_source_index(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    snapshot_id: String,
    query: crate::source_index::SourceIndexQuery,
) -> Result<crate::source_index::SourceIndexPage> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.source_index.clone();
    blocking(move || service.query(&snapshot_id, query)).await
}
#[tauri::command]
pub async fn open_native_agent_hub(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    expected_generation: u64,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let hub = state
        .registry
        .manager_hub()
        .ok_or("manager bridge not initialized")?
        .clone();
    blocking(move || hub.open_agent_hub(&id, expected_generation)).await
}
#[tauri::command]
pub async fn list_human_decisions(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<crate::manager::HumanDecisionSnapshot> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let hub = state
        .registry
        .manager_hub()
        .ok_or("manager bridge not initialized")?
        .clone();
    blocking(move || hub.human_decisions()).await
}
#[tauri::command]
pub async fn respond_human_decision(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    target: crate::manager::HumanDecisionTarget,
    response: serde_json::Value,
) -> Result<serde_json::Value> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let hub = state
        .registry
        .manager_hub()
        .ok_or("manager bridge not initialized")?
        .clone();
    blocking(move || hub.respond_human_decision(&target, response)).await
}
#[tauri::command]
pub async fn list_all_terminals(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<crate::terminal::TerminalOwner>> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let terminals = state.terminals.clone();
    blocking(move || terminals.owners()).await
}
#[tauri::command]
pub async fn list_terminal_history(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: Option<String>,
    cursor: Option<String>,
) -> Result<crate::terminal::HistoryPage> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let terminals = state.terminals.clone();
    blocking(move || terminals.history(id.as_deref(), cursor.as_deref())).await
}
#[tauri::command]
pub async fn read_terminal_script(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    session_id: String,
    execution_id: String,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let terminals = state.terminals.clone();
    blocking(move || terminals.read_script(&id, &session_id, &execution_id)).await
}
#[tauri::command]
pub async fn read_terminal_output(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    session_id: String,
    execution_id: String,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let terminals = state.terminals.clone();
    blocking(move || terminals.read_output(&id, &session_id, &execution_id)).await
}
#[tauri::command]
pub fn attach_command_output(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    generation: u64,
    session_id: String,
    execution_id: String,
    on_data: Channel<crate::terminal::CommandOutput>,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state
        .terminals
        .attach_output(&id, generation, &session_id, &execution_id, on_data)
}
#[tauri::command]
pub async fn kill_terminal_command(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    generation: u64,
    session_id: String,
    execution_id: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let terminals = state.terminals.clone();
    blocking(move || terminals.kill(&id, generation, &session_id, &execution_id)).await
}
#[tauri::command]
pub async fn open_user_terminal(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<crate::terminal::UserShellInfo> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let terminals = state.terminals.clone();
    blocking(move || terminals.open_user_shell(&id)).await
}
#[tauri::command]
pub fn attach_user_terminal(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    shell_id: String,
    on_data: Channel<InvokeResponseBody>,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.terminals.attach_user_shell(&id, &shell_id, on_data)
}
#[tauri::command]
pub fn write_user_terminal(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    shell_id: String,
    data: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.terminals.write_user_shell(&id, &shell_id, &data)
}
#[tauri::command]
pub fn resize_user_terminal(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    shell_id: String,
    cols: u16,
    rows: u16,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state
        .terminals
        .resize_user_shell(&id, &shell_id, cols, rows)
}
#[tauri::command]
pub async fn close_user_terminal(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    shell_id: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
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
pub fn list_plan_backups(
    state: State<'_, AppState>,
    id: String,
) -> Result<Vec<crate::plan::PlanBackup>> {
    state.registry.inspect(&id, Origin::Human)?;
    crate::plan::list_backups(&state.registry.plan_folder(&id))
}
#[tauri::command]
pub async fn restore_plan_backup(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    backup_id: String,
) -> Result<()> {
    let registry = state.registry.clone();
    blocking(move || {
        registry
            .manager_hub()
            .ok_or("manager bridge not initialized")?
            .restore_plan_backup(&id, &backup_id)?;
        publish(&app, &registry);
        Ok(())
    })
    .await
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
pub async fn list_agent_files(
    state: State<'_, AppState>,
    kind: crate::instance::InstanceKind,
) -> Result<crate::setup::AgentFiles> {
    let registry = state.registry.clone();
    blocking(move || Ok(crate::setup::agent_files(&registry, kind))).await
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
    blocking(move || crate::setup::open_folder(&registry, scope, project_instance_id.as_deref()))
        .await
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
pub async fn get_code_diff(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    mode: crate::code_diff::DiffMode,
) -> Result<crate::code_diff::DiffOverview> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || diff.overview(&id, Path::new(&diff_project(&registry, &id)?), mode)).await
}
#[tauri::command]
pub async fn read_code_diff_file(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    snapshot_id: String,
    path: String,
    group: Option<crate::code_diff::DiffGroup>,
) -> Result<crate::code_diff::DiffContent> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || {
        diff.read_file(
            &id,
            Path::new(&diff_project(&registry, &id)?),
            &snapshot_id,
            &path,
            group,
        )
    })
    .await
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || {
        diff.save_file(
            &id,
            Path::new(&diff_project(&registry, &id)?),
            &snapshot_id,
            &path,
            group,
            expected_hash.as_deref(),
            &text,
        )
    })
    .await
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
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || {
        diff.change_file(
            &id,
            Path::new(&diff_project(&registry, &id)?),
            &snapshot_id,
            &path,
            group,
            action,
        )
    })
    .await
}
#[tauri::command]
pub async fn commit_code_diff(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    snapshot_id: String,
    message: String,
) -> Result<crate::code_diff::CommitResult> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || {
        diff.commit(
            &id,
            Path::new(&diff_project(&registry, &id)?),
            &snapshot_id,
            &message,
        )
    })
    .await
}
#[tauri::command]
pub async fn publish_code_diff(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    snapshot_id: String,
    options: crate::code_diff::PublishOptions,
) -> Result<crate::code_diff::PublishResult> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || {
        diff.publish(
            &id,
            Path::new(&diff_project(&registry, &id)?),
            &snapshot_id,
            &options,
        )
    })
    .await
}
#[tauri::command]
pub async fn open_code_diff_pull_request(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let diff = state.code_diff.clone();
    blocking(move || diff.open_pull_request(&id)).await
}
#[tauri::command]
pub async fn get_git_graph(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    limit: u32,
) -> Result<crate::git_graph::GitGraph> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::graph(Path::new(&diff_project(&registry, &id)?), limit))
        .await
}
#[tauri::command]
pub async fn get_commit_diff(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    base: Option<String>,
    target: String,
) -> Result<crate::code_diff::DiffOverview> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, diff) = (state.registry.clone(), state.code_diff.clone());
    blocking(move || {
        diff.commit_overview(
            &id,
            Path::new(&diff_project(&registry, &id)?),
            base.as_deref(),
            &target,
        )
    })
    .await
}
#[tauri::command]
pub async fn revert_commit(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    commit: String,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::revert(Path::new(&diff_project(&registry, &id)?), &commit))
        .await
}
#[tauri::command]
pub async fn cherry_pick_commit(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    commit: String,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        crate::git_graph::cherry_pick(Path::new(&diff_project(&registry, &id)?), &commit)
    })
    .await
}
#[tauri::command]
pub async fn reset_last_commit(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::git_graph::reset_last(Path::new(&diff_project(&registry, &id)?))).await
}
#[tauri::command]
pub async fn create_branch_at(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    name: String,
    commit: Option<String>,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || {
        crate::git_graph::create_branch(
            Path::new(&diff_project(&registry, &id)?),
            &name,
            commit.as_deref(),
        )
    })
    .await
}
#[tauri::command]
pub async fn explain_git_change(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
    kind: String,
    base: Option<String>,
    target: String,
) -> Result<String> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let registry = state.registry.clone();
    blocking(move || crate::explain::explain(&registry, &id, &kind, base.as_deref(), &target)).await
}
#[tauri::command]
pub async fn get_code_quality_state(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || {
        service.refresh_ownership()?;
        Ok(service.state())
    })
    .await
}
#[tauri::command]
pub async fn check_code_quality_prerequisites(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.check_prerequisites()).await
}
#[tauri::command]

pub async fn configure_code_quality(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    config: QualityConfig,
    expected_revision: u64,
    credential: Option<QualityCredentialUpdate>,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.configure(config, expected_revision, credential)).await
}
#[tauri::command]

pub async fn install_code_quality_runtime(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    runtime: QualityRuntime,
    expected_revision: u64,
    consent: bool,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.install_runtime(runtime, expected_revision, consent)).await
}
#[tauri::command]

pub async fn enable_code_quality_windows_features(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    expected_revision: u64,
    consent: bool,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.enable_windows_features(expected_revision, consent)).await
}
#[tauri::command]

pub async fn start_code_quality_service(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    expected_revision: u64,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.start_service(expected_revision)).await
}
#[tauri::command]

pub async fn setup_code_quality(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    expected_revision: u64,
    consent: bool,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.setup(expected_revision, consent)).await
}
#[tauri::command]
pub async fn enter_code_quality(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.enter()).await
}
#[tauri::command]
pub async fn take_over_code_quality(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    consent: bool,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.take_over(consent)).await
}
#[tauri::command]
pub async fn shutdown_code_quality_server(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.stop_server()).await
}
#[tauri::command]
pub async fn reveal_code_quality_credentials(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<crate::code_quality::credentials::QualityLoginDetails> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.reveal_credentials()).await
}
#[tauri::command]
pub async fn set_code_quality_start_at_launch(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.set_start_at_launch(enabled)).await
}
#[tauri::command]

pub async fn save_code_quality_report(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scan_id: String,
    path: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.save_report(&scan_id, &path)).await
}
#[tauri::command]

pub async fn acknowledge_code_quality_rules(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    manifest_hash: String,
    license_hash: String,
    expected_revision: u64,
    consent: bool,
) -> Result<QualityState> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || {
        service.acknowledge_rules(manifest_hash, license_hash, expected_revision, consent)
    })
    .await
}
#[tauri::command]
pub async fn detect_code_quality_project(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<QualityProjectDetection> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.detect_project(&folder)).await
}
#[tauri::command]
pub async fn prepare_code_quality_scan(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    project: QualityProject,
) -> Result<QualityScanPreparation> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.prepare_scan(&project)).await
}
#[tauri::command]

pub async fn start_code_quality_scan(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    project: QualityProject,
    recipe_hash: String,
    consent: bool,
) -> Result<ScanAdmission> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.start_scan(project, recipe_hash, consent)).await
}
#[tauri::command]

pub async fn read_code_quality_scan(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scan_id: String,
) -> Result<QualityScan> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.read_scan(&scan_id)).await
}
#[tauri::command]
pub async fn list_code_quality_scans(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<Vec<QualityScan>> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.list_scans(&folder)).await
}
#[tauri::command]
pub async fn list_code_quality_project_folders(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<String>> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.list_project_folders()).await
}
#[tauri::command]

pub async fn cancel_code_quality_scan(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scan_id: String,
) -> Result<QualityScan> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.cancel_scan(&scan_id)).await
}
#[tauri::command]

pub async fn read_code_quality_report(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scan_id: String,
    report_name: String,
) -> Result<ReportPreview> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.read_report(&scan_id, &report_name)).await
}
#[tauri::command]

pub async fn open_code_quality_dashboard(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scan_id: Option<String>,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.open_dashboard(scan_id.as_deref())).await
}
#[tauri::command]
pub async fn open_code_quality_sso_login(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    url: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let service = state.code_quality.clone();
    blocking(move || service.open_sso_login(&url)).await
}
fn require_main_window(window: &tauri::WebviewWindow) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    Ok(())
}
#[tauri::command]
pub async fn get_knowledge_capabilities(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: Option<String>,
) -> Result<knowledge::KnowledgeCapabilities> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.get_knowledge_capabilities(folder.as_deref())).await
}
#[tauri::command]
pub async fn list_knowledge_vaults(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<knowledge::VaultBinding>> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.list_knowledge_vaults()).await
}
#[tauri::command]
pub async fn prepare_knowledge_vault(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scope: knowledge::ScopeRequest,
    folder: Option<String>,
    destination: String,
    mode: knowledge::VaultMode,
) -> Result<knowledge::VaultPreparation> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.prepare_knowledge_vault(scope, folder.as_deref(), &destination, mode)).await
}
#[tauri::command]
pub async fn confirm_knowledge_vault(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    token: String,
) -> Result<knowledge::VaultBinding> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.confirm_knowledge_vault(&token)).await
}
#[tauri::command]
pub async fn open_knowledge_note(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    vault_id: String,
    path: Option<String>,
) -> Result<knowledge::OpenNoteResult> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.open_knowledge_note(&vault_id, path.as_deref())).await
}
#[tauri::command]
pub async fn open_obsidian_vault_manager(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<()> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.open_obsidian_vault_manager()).await
}
#[tauri::command]
pub async fn open_knowledge_link(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    url: String,
) -> Result<()> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.open_knowledge_link(&url)).await
}
#[tauri::command]
pub async fn open_obsidian_download(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<()> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.open_obsidian_download()).await
}
#[tauri::command]
pub async fn record_obsidian_decline(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<()> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.record_obsidian_decline(&folder)).await
}
#[tauri::command]
pub async fn configure_obsidian_executable(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    executable: Option<String>,
) -> Result<knowledge::ObsidianCapability> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.configure_obsidian_executable(executable.as_deref())).await
}
#[tauri::command]
pub async fn list_vault_entries(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    vault_id: String,
    prefix: String,
    offset: usize,
    limit: usize,
) -> Result<knowledge::VaultEntryPage> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.list_vault_entries(&vault_id, &prefix, offset, limit)).await
}
#[tauri::command]
pub async fn read_vault_document(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    vault_id: String,
    path: String,
    offset: u64,
    expected_hash: Option<String>,
) -> Result<knowledge::VaultDocumentPage> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.read_vault_document(&vault_id, &path, offset, expected_hash.as_deref()))
        .await
}
#[tauri::command]
pub async fn query_vault_links(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    vault_id: String,
    query: knowledge::VaultLinkQuery,
) -> Result<knowledge::VaultQueryAnswer> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.query_vault_links(&vault_id, &query)).await
}
#[tauri::command]
pub async fn refresh_vault_index(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    vault_id: String,
) -> Result<knowledge::VaultRefresh> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.refresh_vault_index(&vault_id)).await
}
#[tauri::command]
pub async fn open_graphify_artifact(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
) -> Result<knowledge::GraphifyArtifact> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.open_graphify_artifact(&folder)).await
}
#[tauri::command]
pub async fn register_graphify_output(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    directory: String,
) -> Result<knowledge::KnowledgeCapabilities> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.register_graphify_output(&folder, &directory)).await
}
#[tauri::command]
pub async fn read_graphify_graph(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    artifact_id: String,
    section: knowledge::GraphifySection,
    offset: usize,
    limit: usize,
) -> Result<knowledge::GraphifyPage> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.read_graphify_graph(&artifact_id, section, offset, limit)).await
}
#[tauri::command]
pub async fn query_graphify_graph(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    artifact_id: String,
    query: knowledge::GraphifyQuery,
) -> Result<knowledge::GraphifyQueryPage> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.query_graphify_graph(&artifact_id, &query)).await
}
#[tauri::command]
pub async fn open_cognee_ui(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<knowledge::CogneeOpenResult> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || Ok(s.open_cognee_ui())).await
}
#[tauri::command]
pub async fn open_host_monitor(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    instance_id: String,
    expected_generation: u64,
) -> Result<crate::host_monitor::HostMonitorOpen> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let monitor = state.host_monitor.clone();
    blocking(move || monitor.open(&instance_id, expected_generation)).await
}
#[tauri::command]
pub async fn collect_host_monitor(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    view_id: String,
    displayed_snapshot_id: Option<String>,
) -> Result<crate::host_monitor::HostSnapshotSummary> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let monitor = state.host_monitor.clone();
    blocking(move || monitor.collect(&view_id, displayed_snapshot_id.as_deref())).await
}
#[tauri::command]
pub async fn read_host_monitor(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    snapshot_id: String,
    query: crate::host_monitor::HostMonitorQuery,
) -> Result<crate::host_monitor::HostMonitorPage> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let monitor = state.host_monitor.clone();
    blocking(move || monitor.read(&snapshot_id, &query)).await
}
#[tauri::command]
pub fn close_host_monitor(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    view_id: String,
) -> Result<()> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    state.host_monitor.close(&view_id);
    Ok(())
}
#[tauri::command]
pub async fn explain_host_monitor(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    snapshot_id: String,
) -> Result<crate::host_monitor::ExplainResult> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (monitor, registry) = (state.host_monitor.clone(), state.registry.clone());
    blocking(move || Ok(monitor.explain_host_monitor(&registry, &snapshot_id))).await
}
#[tauri::command]
pub async fn explain_host_monitor_after_srs_pause(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    snapshot_id: String,
    job_id: String,
    attempt_id: String,
) -> Result<crate::host_monitor::ExplainResult> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (monitor, registry) = (state.host_monitor.clone(), state.registry.clone());
    blocking(move || {
        Ok(monitor.explain_host_monitor_after_srs_pause(
            &registry,
            &snapshot_id,
            &job_id,
            &attempt_id,
        ))
    })
    .await
}
#[tauri::command]
pub fn cancel_host_monitor_explanation(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    snapshot_id: String,
) -> Result<bool> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    Ok(state
        .host_monitor
        .cancel_host_monitor_explanation(&snapshot_id))
}
#[tauri::command]
pub async fn get_agent_markdown_status(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    target: crate::agent_markdown::KnowledgeAssetTarget,
) -> Result<crate::agent_markdown::MarkdownStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, service) = (state.registry.clone(), state.agent_markdown.clone());
    blocking(move || service.status(&registry, target)).await
}
#[tauri::command]

pub async fn prepare_agent_markdown_import(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    target: crate::agent_markdown::KnowledgeAssetTarget,
    selected_native_ids: Option<Vec<String>>,
) -> Result<crate::agent_markdown::MarkdownPreparation> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, service) = (state.registry.clone(), state.agent_markdown.clone());
    blocking(move || service.prepare_import(&registry, target, selected_native_ids)).await
}
#[tauri::command]
pub async fn confirm_agent_markdown_import(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    token: String,
) -> Result<crate::agent_markdown::MarkdownStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, service) = (state.registry.clone(), state.agent_markdown.clone());
    blocking(move || service.confirm_import(&registry, &token)).await
}
#[tauri::command]

pub async fn prepare_agent_markdown_apply(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    target: crate::agent_markdown::KnowledgeAssetTarget,
    source_revision: String,
) -> Result<crate::agent_markdown::MarkdownPreparation> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, service) = (state.registry.clone(), state.agent_markdown.clone());
    blocking(move || service.prepare_apply(&registry, target, &source_revision)).await
}
#[tauri::command]

pub async fn apply_agent_markdown(
    app: AppHandle,
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    token: String,
    selected_ids: Vec<String>,
) -> Result<crate::agent_markdown::MarkdownStatus> {
    if window.label() != "main" {
        return Err("unauthorized window".into());
    }
    let (registry, service) = (state.registry.clone(), state.agent_markdown.clone());
    blocking(move || {
        let result = service.apply(&registry, &token, selected_ids);
        publish(&app, &registry);
        result
    })
    .await
}
#[tauri::command]
pub async fn check_graphify_artifact(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    artifact_id: String,
) -> Result<knowledge::GraphifyArtifactCheck> {
    require_main_window(&window)?;
    let s = state.knowledge.clone();
    blocking(move || s.check_graphify_artifact(&artifact_id)).await
}
#[tauri::command]

pub async fn prepare_srs_scope(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    vault_id: String,
    evidence_folders: Vec<String>,
    exclusions: Vec<String>,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.prepare_srs_scope(&folder, &vault_id, evidence_folders, exclusions)).await
}
#[tauri::command]

pub async fn start_srs(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    scope_token: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.start_srs(&scope_token)).await
}
#[tauri::command]

pub async fn get_srs_job(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.get_srs_job(&job_id)).await
}
#[tauri::command]
pub async fn list_srs_jobs(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    folder: String,
    offset: usize,
    limit: usize,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.list_srs_jobs(&folder, offset, limit)).await
}
#[tauri::command]

pub async fn pause_srs(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.pause_srs(&job_id)).await
}
#[tauri::command]

pub async fn cancel_srs(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.cancel_srs(&job_id)).await
}
#[tauri::command]

pub async fn prepare_srs_resume(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
    mode: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.prepare_srs_resume(&job_id, &mode)).await
}
#[tauri::command]
pub async fn resume_srs(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    token: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.resume_srs(&token)).await
}
#[tauri::command]

pub async fn dispose_srs_item(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
    expected_revision: u64,
    item_id: String,
    disposition: String,
    rationale: String,
    answer: Option<crate::srs::Answer>,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || {
        s.dispose_srs_item(
            &job_id,
            expected_revision,
            &item_id,
            &disposition,
            &rationale,
            answer,
        )
    })
    .await
}
#[tauri::command]

pub async fn query_srs_job(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
    section: String,
    offset: u64,
    limit: u64,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.query_srs_job(&job_id, &section, offset, limit)).await
}
#[tauri::command]

pub async fn open_srs_documents(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.open_srs_documents(&job_id)).await
}
#[tauri::command]

pub async fn rebind_srs_vault(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    job_id: String,
    vault_id: String,
) -> Result<serde_json::Value> {
    require_main_window(&window)?;
    let s = state.srs.clone();
    blocking(move || s.rebind_srs_vault(&job_id, &vault_id)).await
}
/// One tick while the app is closing: false once no entered workspace work is left.
fn tick_closing(app: &AppHandle, registry: &Registry) -> bool {
    use tauri::Manager as _;
    if !app
        .try_state::<AppState>()
        .is_some_and(|state| state.workspace.has_active_work())
    {
        return false;
    }
    // Retain the existing process-exit monitor until entered workspace work settles.
    if let Some(hub) = registry.manager_hub() {
        hub.observe_processes(&registry.snapshot());
    }
    true
}
/// Local-only reconciliation of unfinished project security reviews; AppState is managed shortly after tick starts.
fn reconcile_project_reviews(app: &AppHandle, registry: &Registry) {
    use tauri::Manager as _;
    if let Some(state) = app.try_state::<AppState>() {
        state
            .dependency_project_review
            .reconcile(registry, &state.dependency_security);
    }
}
fn tick_running(
    app: &AppHandle,
    registry: &Registry,
    terminals: &crate::terminal::TerminalService,
) {
    if let Err(e) = registry.refresh() {
        eprintln!("session refresh: {e}");
    }
    let rows = registry.snapshot();
    if let Some(hub) = registry.manager_hub() {
        hub.observe_processes(&rows);
    }
    reconcile_project_reviews(app, registry);
    let _ = app.emit("instances", rows);
    if let Some(browser) = registry.browser_supervisor() {
        if let Some(update) = browser.take_update() {
            let _ = app.emit("browser_state", update);
        }
    }
    terminals.poll_shells();
}
pub fn tick(
    app: AppHandle,
    registry: Arc<Registry>,
    terminals: Arc<crate::terminal::TerminalService>,
    closing: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || loop {
        if closing.load(std::sync::atomic::Ordering::Relaxed) {
            if !tick_closing(&app, &registry) {
                break;
            }
        } else {
            tick_running(&app, &registry, &terminals);
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    });
}
