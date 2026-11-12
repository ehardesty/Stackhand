use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

use super::*;

fn shared() -> Shared {
    Shared {
        state: Mutex::new(QueueState::default()),
        ready: Condvar::new(),
        space: Condvar::new(),
        shutdown: AtomicBool::new(false),
        stop: SessionStop::default(),
    }
}

fn mouse(kind: MouseEventKind, column: u16) -> HostAction {
    HostAction::Input(Event::Mouse(MouseEvent {
        kind,
        column,
        row: 10,
        modifiers: KeyModifiers::NONE,
    }))
}

fn quit() -> HostAction {
    HostAction::Input(Event::Key(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::CONTROL,
    )))
}

#[test]
fn repeated_wheels_use_one_bounded_queue_entry() {
    let shared = shared();
    let wheel = mouse(MouseEventKind::ScrollUp, 20);
    for _ in 0..10_000 {
        push(&shared, wheel.clone(), || Ok(()));
    }
    let state = lock(&shared.state);
    assert_eq!(state.actions, VecDeque::from([(wheel, 10_000)]));
}

#[test]
fn movement_keeps_only_the_latest_position() {
    let shared = shared();
    push(&shared, mouse(MouseEventKind::Moved, 20), || Ok(()));
    let latest = mouse(MouseEventKind::Moved, 40);
    push(&shared, latest.clone(), || Ok(()));
    let state = lock(&shared.state);
    assert_eq!(state.actions, VecDeque::from([(latest, 1)]));
}

#[test]
fn current_input_replaces_stale_wheels_when_the_queue_is_full() {
    let shared = shared();
    for index in 0..INPUT_QUEUE_LIMIT {
        let direction = if index % 2 == 0 {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        };
        push(&shared, mouse(direction, index as u16), || Ok(()));
    }

    push(&shared, quit(), || Ok(()));

    let state = lock(&shared.state);
    assert_eq!(state.actions.len(), INPUT_QUEUE_LIMIT);
    assert_eq!(state.actions.back(), Some(&(quit(), 1)));
    assert_eq!(
        state.actions.front().unwrap().0,
        mouse(MouseEventKind::ScrollDown, 1)
    );
}

#[test]
fn opposite_wheels_keep_order_until_overload_requires_shedding() {
    let shared = shared();
    let up = mouse(MouseEventKind::ScrollUp, 20);
    let down = mouse(MouseEventKind::ScrollDown, 20);
    push(&shared, up.clone(), || Ok(()));
    push(&shared, down.clone(), || Ok(()));
    push(&shared, up.clone(), || Ok(()));

    let state = lock(&shared.state);
    assert_eq!(
        state.actions,
        VecDeque::from([(up.clone(), 1), (down, 1), (up, 1)])
    );
}

#[test]
fn receive_wakes_for_new_input() {
    let shared = Arc::new(shared());
    let scheduler = InputScheduler {
        shared: Arc::clone(&shared),
        reader: None,
    };
    let producer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(10));
        push(&shared, quit(), || Ok(()));
    });

    let batch = scheduler.receive(Duration::from_secs(1)).unwrap();

    assert_eq!(batch.actions, vec![(quit(), 1)]);
    producer.join().unwrap();
}

#[test]
fn receive_returns_after_its_timeout() {
    let scheduler = InputScheduler {
        shared: Arc::new(shared()),
        reader: None,
    };
    let started = Instant::now();

    let batch = scheduler.receive(Duration::from_millis(10)).unwrap();

    assert!(batch.is_empty());
    assert!(started.elapsed() >= Duration::from_millis(5));
}

#[test]
fn receive_reports_reader_errors() {
    let shared = Arc::new(shared());
    lock(&shared.state).error = Some("terminal input failed: test".to_string());
    let scheduler = InputScheduler {
        shared,
        reader: None,
    };

    let error = scheduler.receive(Duration::from_secs(1)).unwrap_err();

    assert_eq!(error.to_string(), "terminal input failed: test");
}

#[test]
fn full_queue_still_checks_framing_faults_and_wakes_the_consumer() {
    let shared = Arc::new(shared());
    for _ in 0..INPUT_QUEUE_LIMIT {
        push(&shared, quit(), || Ok(()));
    }
    let producer_shared = shared.clone();
    let producer = std::thread::spawn(move || {
        let started = Instant::now();
        assert!(!push(&producer_shared, HostAction::PasteRejected, || {
            if started.elapsed() >= Duration::from_millis(20) {
                producer_shared
                    .stop
                    .fail(crate::host::HostFault::UnterminatedPaste);
                Err(std::io::Error::other("expired paste"))
            } else {
                Ok(())
            }
        }));
    });
    producer.join().unwrap();
    let scheduler = InputScheduler {
        shared,
        reader: None,
    };
    assert!(scheduler.receive(Duration::ZERO).is_err());
}
