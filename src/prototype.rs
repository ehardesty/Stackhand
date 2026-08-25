//! Internal prototype validation entry points.
//!
//! These exports support the executable fixture modes and integration tests.
//! They are not a stable Stackhand library interface.

pub use crate::fixtures::{
    run_fixture_input, run_fixture_paste, run_fixture_rendering, run_fixture_round_trip,
};
pub use crate::mouse_fixture::run as run_fixture_mouse;
pub use crate::scrollback_fixture::run as run_fixture_scrollback;
pub use crate::stress::{BlockedInputReport, StressReport};
pub use crate::stress::{run_blocked_input_output_fixture, run_sustained_output_fixture};
