//! The ingestion proof of the interaction fixture, extracted so the
//! fixture module stays under the line cap.
//!
//! Moving the selection around must not stop output ingestion for any
//! Process: the move is routed through the currently selected pane and
//! applied only from the drained request, clamped at the list ends, and
//! every tick counter keeps climbing. The restarts above opened new
//! Runs, so the proof follows each Process's current Run view.
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::console::{ConsoleInteraction, PipeScroll, SelectionMove};
use crate::interaction_fixture::{
    WAIT, apply_move, console_text, last_tick, module_text, wait_for, wait_for_tick,
};
use crate::output::OutputViews;
use crate::supervisor::{Consoles, ProcessId, SupervisorHandle};

/// The fixture process indexes the ingestion proof needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FixtureIndexes {
    pub(crate) focused: usize,
    pub(crate) mute: usize,
    pub(crate) piped: usize,
    pub(crate) oneoff: usize,
}

pub(crate) fn prove_ingest(
    console: &mut ConsoleInteraction,
    pipe_scroll: &mut [Option<PipeScroll>],
    consoles: &Consoles,
    outputs: &OutputViews,
    supervisor: &SupervisorHandle,
    indexes: FixtureIndexes,
    selected: &mut usize,
) -> Result<()> {
    let ingest_snapshot = wait_for(supervisor, WAIT, |_| true)?;
    let focused_live = consoles
        .view_process(
            ingest_snapshot.processes[indexes.focused].process_id,
            ingest_snapshot.processes[indexes.focused]
                .current_run
                .expect("the restarted Process keeps a live Run"),
        )
        .expect("the restarted Process has a live console");
    let mute_live = consoles
        .view_process(
            ingest_snapshot.processes[indexes.mute].process_id,
            ingest_snapshot.processes[indexes.mute]
                .current_run
                .expect("the restarted Process keeps a live Run"),
        )
        .expect("the restarted Process has a live console");
    let base_focused = wait_for_tick(&focused_live, "tick-", 2)?;
    let base_mute = wait_for_tick(&mute_live, "tick-", 2)?;
    // The pipe module retains chunks from earlier Runs, whose counters sit
    // above the restarted counter, so progress is proven at the tail.
    let piped_id = ingest_snapshot.processes[indexes.piped].process_id;
    wait_for_module_tick(outputs, piped_id, "pipe-tick-", 2)?;
    let base_piped = last_tick(&module_text(outputs, piped_id), "pipe-tick-").unwrap_or(0);
    let moves = [
        SelectionMove::Down,
        SelectionMove::Down,
        SelectionMove::Down,
        SelectionMove::Up,
        SelectionMove::Up,
    ];
    let expected = [
        indexes.mute,
        indexes.piped,
        indexes.oneoff,
        indexes.piped,
        indexes.mute,
    ];
    for (direction, expected_selected) in moves.iter().zip(expected.iter()) {
        apply_move(
            console,
            pipe_scroll,
            consoles,
            outputs,
            &ingest_snapshot,
            selected,
            *direction,
        );
        assert_eq!(
            *selected, *expected_selected,
            "the selection must move and clamp exactly like the app"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(
        crate::interaction_fixture::max_tick(&console_text(&focused_live), "tick-").unwrap_or(0)
            > base_focused,
        "selection moves stopped ingestion into the focused terminal"
    );
    assert!(
        crate::interaction_fixture::max_tick(&console_text(&mute_live), "tick-").unwrap_or(0)
            > base_mute,
        "selection moves stopped ingestion into the muted terminal"
    );
    assert!(
        last_tick(&module_text(outputs, piped_id), "pipe-tick-").unwrap_or(0) > base_piped,
        "selection moves stopped ingestion into the pipe module"
    );
    Ok(())
}

fn wait_for_module_tick(
    outputs: &OutputViews,
    piped: ProcessId,
    prefix: &str,
    minimum: u32,
) -> Result<u32> {
    let deadline = Instant::now() + WAIT;
    loop {
        let value = last_tick(&module_text(outputs, piped), prefix).unwrap_or(0);
        if value >= minimum {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            bail!("the pipe module never reached {prefix}{minimum}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
