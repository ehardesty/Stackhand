mod footer;
mod mouse;
mod profile_menu;
mod restore;
mod theme;
mod view;

pub(crate) use footer::{VisibleAction, VisibleActionEvent, VisibleActions};
pub use mouse::MouseRouter;
pub(crate) use profile_menu::{ProjectProfileMenu, ProjectProfileMenuAction};
pub use restore::OuterTerminal;
pub(crate) use theme::LifecycleTone;
#[cfg(test)]
pub(crate) use view::render_project;
pub(crate) use view::render_project_with_search;
pub use view::{
    ConsolePaneKind, ConsoleScrollbar, ConsoleViewMode, ConsoleViewState, ConsoleWarning, PipeLine,
    PortListView, ProcessRowView, pane_inner, process_port_at, process_row_at,
    project_console_geometry, project_layout,
};
