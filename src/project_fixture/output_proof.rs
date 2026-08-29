//! Output proofs for the integrated Project fixture.

use anyhow::{Result, anyhow, bail, ensure};

use crate::supervisor::{OutputViews, ProcessId, ProjectSnapshot};

use super::{CONSOLE_PROOFS, OUTPUT_WAIT, PIPE_PROOFS, POLL, current_run, process};

pub(super) fn prove_console_output(
    snapshot: &ProjectSnapshot,
    consoles: &crate::supervisor::Consoles,
) -> Result<()> {
    for (name, needle) in CONSOLE_PROOFS {
        let process = process(snapshot, name);
        let run_id = current_run(snapshot, name)?;
        let view = consoles
            .view_process(process.process_id, run_id)
            .ok_or_else(|| anyhow!("no live console view for {name}"))?;
        wait_for_console_text(view, needle)?;
    }
    Ok(())
}

pub(super) fn prove_pipe_output(snapshot: &ProjectSnapshot, outputs: &OutputViews) -> Result<u32> {
    for (name, needle, stream) in PIPE_PROOFS {
        let process = process(snapshot, name);
        let run_id = current_run(snapshot, name)?;
        let module = outputs
            .for_process_id(process.process_id)
            .ok_or_else(|| anyhow!("no retained output module for {name}"))?;
        wait_for_retained_text(&module, *stream, needle, run_id)?;
    }
    let process = process(snapshot, "piped");
    retained_pid(
        outputs,
        process.process_id,
        process.name.as_str(),
        "fixture-descendant-pid-",
    )
}

pub(super) fn prove_direct_output(snapshot: &ProjectSnapshot, outputs: &OutputViews) -> Result<()> {
    let process = process(snapshot, "direct");
    let run_id = process
        .recent_runs
        .first()
        .ok_or_else(|| anyhow!("the direct One-shot has no Run summary"))?
        .run_id;
    let module = outputs
        .for_process_id(process.process_id)
        .ok_or_else(|| anyhow!("the direct Process output module is missing"))?;
    wait_for_retained_text(
        &module,
        crate::runtime::OutputStream::Stdout,
        "fixture-direct-command",
        run_id,
    )
}

pub(super) fn prove_noisy_output(outputs: &OutputViews, process_id: ProcessId) -> Result<()> {
    let module = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the noisy Process output module is missing"))?;
    let snapshot = module.snapshot();
    let bytes = retained_bytes(&snapshot);
    ensure!(
        bytes <= crate::supervisor::RETAINED_BYTES,
        "noisy Process output exceeded its bound: {bytes}"
    );
    ensure!(
        snapshot.chunks.iter().any(|chunk| {
            matches!(chunk, crate::supervisor::RetainedChunk::Data { text, .. } if text.contains("fixture-noisy"))
        }),
        "noisy Process output did not reach retained history"
    );
    Ok(())
}

pub(super) fn prove_restart_output(outputs: &OutputViews, process_id: ProcessId) -> Result<()> {
    let module = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the restart Process output module is missing"))?;
    let snapshot = module.snapshot();
    let marker_count = snapshot
        .chunks
        .iter()
        .filter(|chunk| matches!(chunk, crate::supervisor::RetainedChunk::Marker { .. }))
        .count();
    ensure!(
        marker_count >= 3,
        "restart output lost Run boundaries: {snapshot:?}"
    );
    ensure!(
        retained_bytes(&snapshot) <= crate::supervisor::RETAINED_BYTES,
        "restart output exceeded its bound"
    );
    Ok(())
}

pub(super) fn retained_pid(
    outputs: &OutputViews,
    process_id: ProcessId,
    name: &str,
    prefix: &str,
) -> Result<u32> {
    let module = outputs
        .for_process_id(process_id)
        .ok_or_else(|| anyhow!("the {name} output module is missing"))?;
    let deadline = std::time::Instant::now() + OUTPUT_WAIT;
    loop {
        let snapshot = module.snapshot();
        if let Some(pid) = snapshot.chunks.iter().find_map(|chunk| match chunk {
            crate::supervisor::RetainedChunk::Data { text, .. } => text
                .split(prefix)
                .nth(1)
                .and_then(|value| value.lines().next())
                .and_then(|value| value.trim().parse::<u32>().ok()),
            crate::supervisor::RetainedChunk::Marker { .. } => None,
        }) {
            return Ok(pid);
        }
        if std::time::Instant::now() >= deadline {
            bail!("the {name} output did not contain a PID marker: {snapshot:?}");
        }
        std::thread::sleep(POLL);
    }
}

fn retained_bytes(snapshot: &crate::supervisor::RetainedOutput) -> usize {
    snapshot
        .chunks
        .iter()
        .map(|chunk| match chunk {
            crate::supervisor::RetainedChunk::Data { text, .. } => text.len(),
            crate::supervisor::RetainedChunk::Marker { label, .. } => label.len(),
        })
        .sum()
}

fn wait_for_retained_text(
    module: &crate::supervisor::ProcessOutput,
    stream: crate::runtime::OutputStream,
    needle: &str,
    run_id: u64,
) -> Result<()> {
    let deadline = std::time::Instant::now() + OUTPUT_WAIT;
    loop {
        let snapshot = module.snapshot();
        let marker_present = snapshot.chunks.iter().any(|chunk| {
            matches!(chunk, crate::supervisor::RetainedChunk::Marker { run_id: marked, .. } if *marked == run_id)
        });
        let proof_present = snapshot.chunks.iter().any(|chunk| {
            matches!(
                chunk,
                crate::supervisor::RetainedChunk::Data {
                    run_id: marked,
                    stream: chunk_stream,
                    text,
                    ..
                } if *marked == run_id && *chunk_stream == stream && text.contains(needle)
            )
        });
        if marker_present && proof_present {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!(
                "the retained proof '{needle}' never reached the module (marker: {marker_present})"
            );
        }
        std::thread::sleep(POLL);
    }
}

fn wait_for_console_text(view: crate::supervisor::ConsoleView, needle: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + OUTPUT_WAIT;
    loop {
        if view
            .snapshot()
            .is_some_and(|snapshot| buffer_text(&snapshot).contains(needle))
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            bail!("the fixture proof '{needle}' never reached the console");
        }
        std::thread::sleep(POLL);
    }
}

fn buffer_text(snapshot: &crate::terminal::OwnedTerminalSnapshot) -> String {
    snapshot
        .buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}
