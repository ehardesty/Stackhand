mod mouse;
mod restore;
mod theme;
mod view;

pub use mouse::MouseRouter;
pub use restore::OuterTerminal;
pub(crate) use theme::LifecycleTone;
pub use view::{
    ConsolePaneKind, ConsoleScrollbar, ConsoleViewMode, ConsoleViewState, ConsoleWarning, PipeLine,
    ProcessRowView, pane_inner, process_row_at, project_console_geometry, project_layout,
    render_project,
};
