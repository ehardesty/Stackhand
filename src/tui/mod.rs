mod mouse;
mod restore;
mod view;

pub use mouse::MouseRouter;
pub use restore::OuterTerminal;
pub use view::{ConsoleViewMode, ConsoleViewState, ConsoleWarning, console_area, render};
