use std::time::Duration;

use anyhow::{Result, ensure};

use crate::fixtures::{start_fixture_run, wait_for_snapshot};
use crate::geometry::TerminalGeometry;
use crate::runtime::SpawnCommand;
use crate::terminal::{MouseButton, MouseKind, MouseModifiers, SelectionPoint, TerminalMouseEvent};

pub fn run() -> Result<()> {
    let geometry = TerminalGeometry::DEFAULT;
    let command = SpawnCommand::new("/bin/sh").arg("-c").arg(
        r#"stty raw -echo
printf '\033[?1003h\033[?1006hmouse-ready\r\n'
IFS= read -r bytes
hex=$(printf '%s' "$bytes" | od -An -tx1 | tr -d ' \n')
printf '\r\nmouse-bytes:%s\r\n' "$hex"
sleep 1"#,
    );
    let mut run = start_fixture_run(command, geometry, None)?;
    let session = run.terminal().expect("mouse fixture is PTY-mode");

    let fixture_result = (|| {
        wait_for_snapshot(&session, "mouse", |snapshot| {
            snapshot.mouse_tracking && snapshot.text().contains("mouse-ready")
        })?;

        // Shift makes Stackhand own the complete selection gesture. These
        // events must not be visible to the child.
        for (kind, col, row) in [
            (MouseKind::Press(MouseButton::Left), 0, 0),
            (MouseKind::Drag(MouseButton::Left), 1, 0),
            (MouseKind::Release(MouseButton::Left), 1, 0),
        ] {
            let _ = session.send_mouse(mouse_event(kind, col, row, true));
        }

        for (kind, col, row) in [
            (MouseKind::Press(MouseButton::Left), 2, 3),
            (MouseKind::Release(MouseButton::Left), 2, 3),
            (MouseKind::Motion, 3, 4),
            (MouseKind::Press(MouseButton::Left), 4, 5),
            (MouseKind::Drag(MouseButton::Left), 5, 6),
            (MouseKind::Release(MouseButton::Left), 5, 6),
            (MouseKind::WheelUp, 6, 7),
            (MouseKind::WheelDown, 7, 8),
            (MouseKind::WheelLeft, 8, 9),
            (MouseKind::WheelRight, 9, 10),
        ] {
            let _ = session.send_mouse(mouse_event(kind, col, row, false));
        }
        let _ = session.send_raw(vec![b'\n']);

        let expected = b"\x1b[<0;3;4M\x1b[<0;3;4m\x1b[<35;4;5M\x1b[<0;5;6M\x1b[<32;6;7M\x1b[<0;6;7m\x1b[<64;7;8M\x1b[<65;8;9M\x1b[<66;9;10M\x1b[<67;10;11M";
        let expected_hex = hex(expected);
        let output = wait_for_snapshot(&session, "mouse", |snapshot| {
            snapshot
                .text()
                .replace('\n', "")
                .contains(&format!("mouse-bytes:{expected_hex}"))
        })?
        .text();
        ensure!(
            output
                .replace('\n', "")
                .contains(&format!("mouse-bytes:{expected_hex}")),
            "child mouse bytes differ from the SGR fixture: {output:?}"
        );
        println!(
            "mouse-fixture: SGR press, release, motion, drag, and four wheel directions reached the child; Shift override bytes did not"
        );
        Ok::<_, anyhow::Error>(())
    })();

    let shutdown_result = run.shutdown();
    fixture_result?;
    shutdown_result?;
    Ok(())
}

fn mouse_event(
    kind: MouseKind,
    col: u16,
    surface_row: i32,
    stackhand_owned: bool,
) -> TerminalMouseEvent {
    TerminalMouseEvent {
        kind,
        point: SelectionPoint { col, surface_row },
        modifiers: MouseModifiers {
            shift: stackhand_owned,
            ..MouseModifiers::default()
        },
        stackhand_owned,
        time: Duration::from_millis(10),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
