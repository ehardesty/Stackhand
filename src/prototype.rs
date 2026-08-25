//! Internal prototype validation entry points.
//!
//! These functions support the executable fixture modes and integration
//! tests. They are not a stable Stackhand library interface.

use anyhow::Result;

pub use crate::stress::{BlockedInputReport, StressReport};

pub fn run_fixture_round_trip(text: &str) -> Result<()> {
    crate::fixtures::run_fixture_round_trip(text)
}

pub fn run_fixture_input() -> Result<()> {
    crate::fixtures::run_fixture_input()
}

pub fn run_fixture_paste() -> Result<()> {
    crate::fixtures::run_fixture_paste()
}

pub fn run_fixture_rendering() -> Result<()> {
    crate::fixtures::run_fixture_rendering()
}

pub fn run_fixture_scrollback() -> Result<()> {
    crate::scrollback_fixture::run()
}

pub fn run_fixture_mouse() -> Result<()> {
    crate::mouse_fixture::run()
}

pub fn run_sustained_output_fixture() -> Result<StressReport> {
    crate::stress::run_sustained_output_fixture()
}

pub fn run_blocked_input_output_fixture() -> Result<BlockedInputReport> {
    crate::stress::run_blocked_input_output_fixture()
}
