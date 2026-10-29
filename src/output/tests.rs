use super::*;

const TEST_HISTORY_BYTES: usize = 128 * 1024;

fn views(count: usize) -> OutputViews {
    OutputViews::with_settings(
        count,
        OutputSettings {
            per_process_history_bytes: TEST_HISTORY_BYTES,
            global_history_bytes: TEST_HISTORY_BYTES * 8,
            ..Default::default()
        },
    )
}

fn output() -> Arc<ProcessOutput> {
    views(1).for_process(0).unwrap()
}

#[test]
fn logs_format_fixed_timestamps_streams_runs_and_invalid_utf8() {
    let output = output();
    output.mark_run_at(1, 3_723_004);
    output.append_at(1, OutputStream::Stdout, 3_723_005, b"ok\n");
    output.append_at(1, OutputStream::Stderr, 3_723_006, &[b'x', 0xff, b'\n']);
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
    output.append_at(1, OutputStream::Combined, 0, &data);
    let lines = output.snapshot().display_lines(10);
    assert!(lines[0].text.contains("[line continued at 16 KiB]"));
    assert!(lines.last().unwrap().text.ends_with("pty: after"));
}

#[test]
fn normalization_spans_reader_chunks_without_breaking_utf8_or_line_bounds() {
    let output = output();
    let euro = "€".as_bytes();
    output.append_at(1, OutputStream::Combined, 0, &euro[..1]);
    output.append_at(1, OutputStream::Combined, 1, &euro[1..]);
    for _ in 0..LOGICAL_LINE_BYTES {
        output.append_at(1, OutputStream::Combined, 2, b"x");
    }
    output.append_at(1, OutputStream::Combined, 3, b"\nafter\n");

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

    output.append_at(1, OutputStream::Stdout, 0, b"later\n");
    let changed = output
        .snapshot_if_changed(Some(first.generation))
        .expect("new output returns a new snapshot");
    assert_ne!(changed.generation, first.generation);
}

#[test]
fn pending_invalid_utf8_cannot_expand_a_snapshot_past_the_process_budget() {
    let output = output();
    output.mark_run(1);
    for stream in [
        OutputStream::Stdout,
        OutputStream::Stderr,
        OutputStream::Combined,
    ] {
        output.append(1, stream, &vec![0xff; 16_000]);
    }
    let snapshot = output.snapshot();
    assert!(snapshot.truncated, "normalization must make expiry visible");
    assert!(snapshot.chunks.retained_bytes() <= TEST_HISTORY_BYTES);
    assert!(snapshot.chunks.iter().any(|chunk| matches!(chunk,
        RetainedChunk::Data { stream: OutputStream::Combined, text, .. } if text.contains('�'))));
    let next = output.snapshot();
    assert_eq!(snapshot.chunks, next.chunks);
    assert_eq!(snapshot.dropped_chunks, next.dropped_chunks);
}

#[test]
fn literal_search_is_case_sensitive_and_bounded_to_retained_text() {
    let output = output();
    output.append_at(1, OutputStream::Stdout, 0, b"needle Needle needle\n");
    let search = output.snapshot().search("needle");
    assert_eq!(search.matches.len(), 2);
    assert!(!search.limited);
    assert!(output.snapshot().search("NEEDLE").matches.is_empty());
}

#[test]
fn per_process_bound_evicts_oldest_and_shows_the_history_marker() {
    let output = output();
    output.append_at(1, OutputStream::Stdout, 0, &vec![b'a'; TEST_HISTORY_BYTES]);
    output.append_at(1, OutputStream::Stdout, 1, b"new\n");
    let snapshot = output.snapshot();
    assert!(snapshot.truncated);
    assert!(
        snapshot
            .chunks
            .iter()
            .any(|chunk| matches!(chunk, RetainedChunk::Data { text, .. } if text == "new\n"))
    );
    assert!(
        snapshot.display_window_from(None, 1).unwrap()[0]
            .text
            .contains("history limit")
    );
}

#[test]
fn project_bound_evicts_the_oldest_process_even_when_another_is_selected() {
    let views = views(9);
    for process in 0..9 {
        views.for_process(process).unwrap().append_at(
            1,
            OutputStream::Stdout,
            process as u64,
            &vec![b'a' + process as u8; TEST_HISTORY_BYTES],
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
        output.append_at(1, OutputStream::Combined, 0, &[*byte]);
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
    output.append_at(1, OutputStream::Stdout, 0, b"\x1b]unfinished title");
    output.append_at(1, OutputStream::Stderr, 1, b"visible error\r\n");
    output.append_at(1, OutputStream::Stdout, 2, b"\x07visible output\n");
    output.append_at(1, OutputStream::Combined, 3, b"\x1b[");
    output.mark_run_at(2, 4);
    output.append_at(2, OutputStream::Combined, 5, b"new run\n");
    // A new Run must also reset a parser when no marker was supplied.
    output.append_at(2, OutputStream::Stdout, 6, b"\x1b]unfinished");
    output.append_at(3, OutputStream::Stdout, 7, b"next run\n");
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
    output.append_at(1, OutputStream::Combined, 0, b"\x1b[?25h\x1b[2F");
    assert!(output.snapshot().chunks.is_empty());
    // Unicode continuation bytes can overlap the 8-bit terminal-control range.
    let trace = "\x1b]8;;https://example.com\x1b\\測試 €\x1b]8;;\x1b\\\r\n";
    for byte in trace.bytes() {
        output.append_at(1, OutputStream::Combined, 1, &[byte]);
    }
    let lines = output.snapshot().display_lines(10);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].text, "00:00:00.001 pty: 測試 €");
}

#[test]
fn pinned_history_stays_immutable_and_charged_until_its_reader_releases_it() {
    let limits = OutputSettings {
        per_process_history_bytes: 64 * 1024,
        global_history_bytes: 128 * 1024,
        ..Default::default()
    };
    let output = OutputViews::with_settings(1, limits)
        .for_process(0)
        .unwrap();
    output.append_at(1, OutputStream::Stdout, 0, &b"original\n".repeat(100));
    let old = output.snapshot();
    let text = old.display_lines(200);
    for _ in 0..20 {
        output.append_at(
            1,
            OutputStream::Stdout,
            1,
            &b"newer retained output\n".repeat(100),
        );
    }
    output.append_at(1, OutputStream::Stdout, 2, b"latest sentinel\n");
    let new = output.snapshot();
    assert_eq!(old.display_lines(200), text);
    assert!(new.search("original").matches.is_empty());
    assert_eq!(new.search("latest sentinel").matches.len(), 1);
    let allocation = Arc::clone(&lock(&output.project).allocation);
    let pinned = allocation.load(Ordering::Relaxed);
    assert!(pinned <= limits.global_history_bytes);
    drop(old);
    assert!(allocation.load(Ordering::Relaxed) < pinned);
}

#[test]
fn a_noisy_process_does_not_evict_a_quiet_processs_older_error() {
    let views = OutputViews::with_settings(
        2,
        OutputSettings {
            per_process_history_bytes: 64 * 1024,
            global_history_bytes: 96 * 1024,
            ..Default::default()
        },
    );
    let quiet = views.for_process(0).unwrap();
    quiet.append_at(
        1,
        OutputStream::Stderr,
        0,
        &b"database startup failed\n".repeat(300),
    );
    let noise = views.for_process(1).unwrap();
    for _ in 0..30 {
        noise.append_at(
            1,
            OutputStream::Stdout,
            1,
            &b"request completed\n".repeat(100),
        );
    }
    assert!(!quiet.snapshot().truncated);
    assert!(
        !quiet
            .snapshot()
            .search("database startup failed")
            .matches
            .is_empty()
    );
    assert!(noise.snapshot().truncated);
}

#[test]
fn indexed_windows_preserve_interleaved_stream_order_across_segments() {
    let output = views(1).for_process(0).unwrap();
    output.append_at(1, OutputStream::Stdout, 0, b"partial");
    for index in 0..600 {
        output.append_at(
            1,
            OutputStream::Stderr,
            1,
            format!("line {index}\n").as_bytes(),
        );
    }
    let with_partial = output.snapshot();
    output.append_at(1, OutputStream::Stdout, 2, b" completed\n");
    let completed = output.snapshot();
    assert!(
        with_partial.display_window_from(None, 1).unwrap()[0]
            .text
            .ends_with("out: partial")
    );
    assert!(
        completed.display_window_from(None, 1).unwrap()[0]
            .text
            .ends_with("out: partial completed")
    );
    let lines = completed.display_lines(1000);
    for (row, line) in lines.iter().enumerate() {
        assert_eq!(completed.display_source_at(row), line.source);
        assert_eq!(
            completed.display_position(line.source),
            Some((row, lines.len()))
        );
        assert_eq!(
            completed.display_window_from(line.source, 1).unwrap()[0],
            *line
        );
    }
}

#[test]
fn incremental_search_completes_and_keeps_the_newest_matches_after_appends() {
    let output = OutputViews::with_settings(
        1,
        OutputSettings {
            per_process_history_bytes: 4 * 1024 * 1024,
            global_history_bytes: 8 * 1024 * 1024,
            ..Default::default()
        },
    )
    .for_process(0)
    .unwrap();
    output.append_at(1, OutputStream::Stdout, 0, b"old unique target\n");
    for _ in 0..100 {
        output.append_at(1, OutputStream::Stdout, 0, &b"ordinary line\n".repeat(100));
    }
    let snapshot = output.snapshot();
    let mut cache = SearchCache::default();
    let mut result = cache.search(&snapshot, "old unique target");
    assert!(result.pending);
    for _ in 0..100 {
        if !result.pending {
            break;
        }
        result = cache.search(&snapshot, "old unique target");
    }
    assert!(!result.pending);
    assert_eq!(result.matches.len(), 1);
    output.append_at(1, OutputStream::Stdout, 1, &b"needle\n".repeat(1200));
    let latest = output.snapshot();
    let capped = cache.search(&latest, "needle");
    assert!(capped.limited);
    assert_eq!(capped.matches.len(), SEARCH_MATCH_LIMIT);
    assert_eq!(
        capped.matches[0].sequence,
        sequence(latest.chunks.iter().next_back().unwrap())
    );
    assert!(
        capped
            .matches
            .windows(2)
            .all(|pair| pair[0].sequence > pair[1].sequence)
    );
}

#[test]
fn cached_search_finds_new_tail_output_after_previous_snapshots_are_released() {
    let output = output();
    output.append(1, OutputStream::Stdout, b"needle old\n");
    let mut cache = SearchCache::default();
    let old_match = {
        let snapshot = output.snapshot();
        let matches = cache.search(&snapshot, "needle").matches;
        assert_eq!(matches.len(), 1);
        matches[0]
    };
    output.append(1, OutputStream::Stdout, b"needle new\n");
    let snapshot = output.snapshot();
    let found = cache.search(&snapshot, "needle");
    assert_eq!(found.matches.len(), 2);
    assert_eq!(found.matches[1], old_match);
    assert!(found.matches[0].sequence > old_match.sequence);
    assert!(!found.pending);
}

#[test]
fn a_window_into_a_multiline_record_preserves_rows_and_copy_coordinates() {
    let snapshot = RetainedOutput {
        chunks: [
            RetainedChunk::Marker {
                run_id: 1,
                label: "Run 1".into(),
                sequence: 0,
                observed_at_ms: 0,
            },
            RetainedChunk::Data {
                run_id: 1,
                stream: OutputStream::Stdout,
                text: "α\n\nlast\n".into(),
                sequence: 1,
                observed_at_ms: 0,
                continued: false,
            },
        ]
        .into_iter()
        .collect(),
        ..Default::default()
    };
    let rows = snapshot.display_lines(10);
    assert_eq!(
        rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
        [
            "00:00:00.000 Run 1",
            "00:00:00.000 out: α",
            "00:00:00.000 out: ",
            "00:00:00.000 out: last"
        ]
    );
    assert!(rows[0].marker);
    assert_eq!(rows[0].content_offset, 0);
    let window = snapshot.display_window_from(Some((1, 1)), 1).unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].text, "00:00:00.000 out: ");
    assert_eq!(window[0].source, Some((1, 1)));
    assert_eq!(window[0].content_offset, 18);
    assert!(!window[0].marker);
    assert!(snapshot.display_window_from(Some((1, 3)), 1).is_none());
}
