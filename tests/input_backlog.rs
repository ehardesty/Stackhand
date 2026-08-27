#![cfg(unix)]

use std::fs;
use std::io::Write;
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
  - name: idle
    kind: service
    autostart: false
    terminal: pipe
    command:
      program: /bin/true
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
    let drain = thread::spawn(move || {
        let mut sink = std::io::sink();
        let _ = std::io::copy(&mut reader, &mut sink);
    });
    let mut writer = pair.master.take_writer().unwrap();
    thread::sleep(Duration::from_secs(1));

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
