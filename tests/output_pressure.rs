use stackhand::prototype::run_output_pressure_fixture;
use stackhand::supervisor::{RETAINED_BYTES, RETAINED_CHUNKS};

/// Several concurrent noisy pipe Processes must stay at their retention
/// bound, keep draining, and never delay lifecycle work for a Process that
/// produces no output.
#[test]
fn noisy_output_stays_bounded_and_lifecycle_commands_keep_flowing() {
    let report = run_output_pressure_fixture().expect("output pressure fixture must pass");

    println!("output-pressure-report: {report:?}");
    assert_eq!(report.noisy_processes, 3);
    // Every noisy Process hit the truncation metadata, so the bound is
    // observable, not just present in the type.
    assert!(
        report.truncated_processes >= 3,
        "expected at least three truncated Processes, got {}",
        report.truncated_processes
    );
    assert!(
        report.max_retained_bytes <= RETAINED_BYTES,
        "retained output exceeded its byte bound"
    );
    assert!(
        report.max_retained_chunks <= RETAINED_CHUNKS,
        "retained output exceeded its chunk bound"
    );
    // A stop issued into a flooded output plane still lands promptly.
    let latency_ms = report.command_latency_ms;
    assert!(
        latency_ms < 10_000,
        "output flood delayed a lifecycle command for {latency_ms} ms"
    );
}
