mod app;
mod byte_budget;
mod config;
mod console;
mod fixtures;
mod geometry;
mod ingest_fixture;
pub mod interaction_fixture;
mod lifecycle_fixture;
mod log_view;
pub mod model;
mod mouse_fixture;
mod output;
mod output_pressure;
mod pipe_scroll;
mod process_logs;
pub mod project_fixture;
pub mod prototype;
mod runtime;
mod scrollback_fixture;
pub mod smoke_fixture;
mod stress;
pub mod supervisor;
mod sync_fixture;
mod terminal;
mod tui;

pub use app::{
    run_discovered_project, run_discovered_project_with_profile,
    run_discovered_project_with_profiles, run_project, run_project_with_profile,
    run_project_with_profiles,
};
pub use config::{
    EffectiveProjectView, ResolutionSources, show_project_with_profiles, validate_project,
    validate_project_sources_with_profiles, validate_project_with_profile,
    validate_project_with_profiles,
};
