#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};

const WHEEL_EVENTS: usize = 10_000;
const INPUT_DELIVERY_BOUND: Duration = Duration::from_secs(2);
const QUIT_BOUND: Duration = Duration::from_secs(2);

#[test]
fn wheel_burst_does_not_hold_later_keyboard_input_behind_redraws() {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config = std::env::temp_dir().join(format!("stackhand-wheel-backlog-{suffix}.yaml"));
    fs::write(
        &config,
        r#"version: 1
processes:
  scrolling:
    kind: service
    terminal:
      mode: pty
      input: focused
    command: [/bin/sh, "-c", "i=0; while :; do if [ $((i % 100)) -eq 0 ]; then echo scroll-ready; fi; echo line-$i; i=$((i+1)); sleep 0.005; done"]
"#,
    )
    .unwrap();

    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_stackhand"));
    command.arg(&config);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let drain = thread::spawn(move || {
        let mut recent = Vec::new();
        let mut buffer = [0; 8 * 1_024];
        let mut reported_ready = false;
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            recent.extend_from_slice(&buffer[..count]);
            if !reported_ready
                && recent
                    .windows(b"scroll-ready".len())
                    .any(|window| window == b"scroll-ready")
            {
                let _ = ready_tx.send(());
                reported_ready = true;
            }
            if recent.len() > 2 * b"scroll-ready".len() {
                recent.drain(..recent.len() - b"scroll-ready".len());
            }
        }
    });
    let mut writer = pair.master.take_writer().unwrap();
    if let Err(error) = ready_rx.recv_timeout(Duration::from_secs(4)) {
        let _ = child.kill();
        let _ = drain.join();
        let _ = fs::remove_file(&config);
        panic!("active PTY pane did not render its readiness marker: {error}");
    }

    let (delivered_tx, delivered_rx) = mpsc::channel();
    let input = thread::spawn(move || {
        let started = Instant::now();
        for index in 0..WHEEL_EVENTS {
            let code = if index < WHEEL_EVENTS / 2 || index % 2 == 0 {
                64
            } else {
                65
            };
            write!(writer, "\x1b[<{code};40;15M").unwrap();
        }
        writer.write_all(b"\x11").unwrap();
        delivered_tx.send(started.elapsed()).unwrap();
    });

    let delivery = match delivered_rx.recv_timeout(INPUT_DELIVERY_BOUND) {
        Ok(delivery) => delivery,
        Err(error) => {
            let _ = child.kill();
            let _ = input.join();
            let _ = drain.join();
            let _ = fs::remove_file(&config);
            panic!("wheel input blocked the later quit key: {error}");
        }
    };
    assert!(
        delivery < INPUT_DELIVERY_BOUND,
        "wheel input took {delivery:?} to drain"
    );

    let quit_deadline = Instant::now() + QUIT_BOUND;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= quit_deadline {
            let _ = child.kill();
            let _ = input.join();
            let _ = drain.join();
            let _ = fs::remove_file(&config);
            panic!("quit stayed behind the drained wheel burst");
        }
        thread::sleep(Duration::from_millis(5));
    }

    input.join().unwrap();
    drain.join().unwrap();
    fs::remove_file(config).unwrap();
}
