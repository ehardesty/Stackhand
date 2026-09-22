use super::*;

fn output() -> Arc<ProcessOutput> {
    OutputViews::new(1).for_process(0).unwrap()
}

#[test]
fn logs_format_fixed_timestamps_streams_runs_and_invalid_utf8() {
    let output = output();
    output.mark_run_at(1, 3_723_004);
    output.append_at(1, OutputStream::Stdout, 3_723_005, b"ok\n".to_vec());
    output.append_at(1, OutputStream::Stderr, 3_723_006, vec![b'x', 0xff, b'\n']);
    let text: Vec<_> = output
        .snapshot()
        .display_lines(10)
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(
        text,
        [
            "01:02:03.004 ── Run 1 started ──",
            "01:02:03.005 out: ok",
            "01:02:03.006 err: x�",
        ]
    );
}

#[test]
fn long_lines_are_split_visibly_and_later_output_survives() {
    let output = output();
    let mut data = vec![b'a'; LOGICAL_LINE_BYTES + 1];
    data.extend_from_slice(b"\nafter\n");
    output.append_at(1, OutputStream::Combined, 0, data);
    let lines = output.snapshot().display_lines(10);
    assert!(lines[0].text.contains("[line continued at 16 KiB]"));
    assert!(lines.last().unwrap().text.ends_with("pty: after"));
}

#[test]
fn normalization_spans_reader_chunks_without_breaking_utf8_or_line_bounds() {
    let output = output();
    let euro = "€".as_bytes();
    output.append_at(1, OutputStream::Combined, 0, euro[..1].to_vec());
    output.append_at(1, OutputStream::Combined, 1, euro[1..].to_vec());
    for _ in 0..LOGICAL_LINE_BYTES {
        output.append_at(1, OutputStream::Combined, 2, vec![b'x']);
    }
    output.append_at(1, OutputStream::Combined, 3, b"\nafter\n".to_vec());

    let lines = output.snapshot().display_lines(10);
    assert!(lines.iter().any(|line| line.text.contains('€')));
    assert!(
        lines
            .iter()
            .any(|line| line.text.contains("[line continued at 16 KiB]"))
    );
    assert!(lines.last().unwrap().text.ends_with("pty: after"));
}

#[test]
fn unchanged_logs_do_not_clone_another_snapshot() {
    let output = output();
    let first = output
        .snapshot_if_changed(None)
        .expect("the first snapshot is returned");

    assert!(output.snapshot_if_changed(Some(first.generation)).is_none());

    output.append_at(1, OutputStream::Stdout, 0, b"later\n".to_vec());
    let changed = output
        .snapshot_if_changed(Some(first.generation))
        .expect("new output returns a new snapshot");
    assert_ne!(changed.generation, first.generation);
}

#[test]
fn literal_search_is_case_sensitive_and_bounded_to_retained_text() {
    let output = output();
    output.append_at(
        1,
        OutputStream::Stdout,
        0,
        b"needle Needle needle\n".to_vec(),
    );
    let search = output.snapshot().search("needle");
    assert_eq!(search.matches.len(), 2);
    assert!(!search.limited);
    assert!(output.snapshot().search("NEEDLE").matches.is_empty());
}

#[test]
fn per_process_bound_evicts_oldest_and_shows_the_history_marker() {
    let output = output();
    output.append_at(1, OutputStream::Stdout, 0, vec![b'a'; RETAINED_BYTES]);
    output.append_at(1, OutputStream::Stdout, 1, b"new\n".to_vec());
    let snapshot = output.snapshot();
    assert!(snapshot.truncated);
    assert!(
        snapshot
            .chunks
            .iter()
            .any(|chunk| matches!(chunk, RetainedChunk::Data { text, .. } if text == "new\n"))
    );
    assert!(
        snapshot.display_lines(100)[0]
            .text
            .contains("history limit")
    );
}

#[test]
fn project_bound_evicts_the_oldest_process_even_when_another_is_selected() {
    let views = OutputViews::new(9);
    for process in 0..9 {
        views.for_process(process).unwrap().append_at(
            1,
            OutputStream::Stdout,
            process as u64,
            vec![b'a' + process as u8; RETAINED_BYTES],
        );
    }
    let oldest = views.for_process(0).unwrap().snapshot();
    let newest = views.for_process(8).unwrap().snapshot();
    assert!(oldest.truncated);
    assert!(!newest.chunks.is_empty());
}

#[test]
fn dotnet_terminal_controls_do_not_leak_into_logs_or_search() {
    let output = output();
    let trace = b"\x1b[?25h\x1b[?25l\x1b[2F\x1b[120G\x1b[7D(14.0s)\r\n\x1b[J  QUADRANT.Domain.Tests test \x1b[36;1mnet10.0\x1b[m \x1b[32;1msucceeded\x1b[m (14.5s)\r\n\x1b[?25l\x1b[1F\x1b[?25hTest summary: total: 2957, failed: 0, \x1b[32;1msucceeded: 2929\x1b[m, skipped: 28, duration: 22.0s\r\nBuild \x1b[32;1msucceeded\x1b[m in 23.2s\r\n";
    // Reader boundaries can fall inside any escape sequence.
    for byte in trace {
        output.append_at(1, OutputStream::Combined, 0, vec![*byte]);
    }
    let snapshot = output.snapshot();
    let lines: Vec<_> = snapshot
        .display_lines(10)
        .into_iter()
        .map(|line| line.text)
        .collect();
    assert_eq!(
        lines,
        [
            "00:00:00.000 pty: (14.0s)",
            "00:00:00.000 pty:   QUADRANT.Domain.Tests test net10.0 succeeded (14.5s)",
            "00:00:00.000 pty: Test summary: total: 2957, failed: 0, succeeded: 2929, skipped: 28, duration: 22.0s",
            "00:00:00.000 pty: Build succeeded in 23.2s",
        ]
    );
    assert_eq!(snapshot.search("Build succeeded").matches.len(), 1);
    assert!(snapshot.search("[32;1m").matches.is_empty());
}

#[test]
fn controls_are_isolated_between_streams_and_runs() {
    let output = output();
    output.mark_run_at(1, 0);
    output.append_at(
        1,
        OutputStream::Stdout,
        0,
        b"\x1b]unfinished title".to_vec(),
    );
    output.append_at(1, OutputStream::Stderr, 1, b"visible error\r\n".to_vec());
    output.append_at(1, OutputStream::Stdout, 2, b"\x07visible output\n".to_vec());
    output.append_at(1, OutputStream::Combined, 3, b"\x1b[".to_vec());
    output.mark_run_at(2, 4);
    output.append_at(2, OutputStream::Combined, 5, b"new run\n".to_vec());
    // A new Run must also reset a parser when no marker was supplied.
    output.append_at(2, OutputStream::Stdout, 6, b"\x1b]unfinished".to_vec());
    output.append_at(3, OutputStream::Stdout, 7, b"next run\n".to_vec());
    let snapshot = output.snapshot();
    for expected in ["visible error", "visible output", "new run", "next run"] {
        assert_eq!(snapshot.search(expected).matches.len(), 1, "{expected}");
    }
    assert!(snapshot.search("unfinished").matches.is_empty());
    assert_eq!(snapshot.display_lines(20).len(), 6);
}

#[test]
fn hyperlink_payloads_and_control_only_chunks_are_not_retained() {
    let output = output();
    output.append_at(1, OutputStream::Combined, 0, b"\x1b[?25h\x1b[2F".to_vec());
    assert!(output.snapshot().chunks.is_empty());
    // Unicode continuation bytes can overlap the 8-bit terminal-control range.
    let trace = "\x1b]8;;https://example.com\x1b\\測試 €\x1b]8;;\x1b\\\r\n";
    for byte in trace.bytes() {
        output.append_at(1, OutputStream::Combined, 1, vec![byte]);
    }
    let lines = output.snapshot().display_lines(10);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "00:00:00.001 pty: 測試 €");
}
