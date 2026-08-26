mod app;
mod config;
mod console;
mod fixtures;
mod geometry;
pub mod model;
mod mouse_fixture;
mod output;
mod output_pressure;
pub mod project_fixture;
pub mod prototype;
mod runtime;
mod scrollback_fixture;
mod stress;
pub mod supervisor;
mod terminal;
mod tui;

pub use app::run_project;
