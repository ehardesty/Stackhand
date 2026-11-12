use super::*;

#[test]
fn dispositions_are_scoped_and_exclusive() {
    const CHILD: &str = "STACKHAND_SIGNAL_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::signals::tests::dispositions_are_scoped_and_exclusive",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    // Global disposition changes live only in this dedicated test subprocess.
    unsafe {
        let mut ignored: libc::sigaction = std::mem::zeroed();
        ignored.sa_sigaction = libc::SIG_IGN;
        ignored.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut ignored.sa_mask);
        libc::sigaddset(&mut ignored.sa_mask, libc::SIGUSR1);
        assert_eq!(
            libc::sigaction(libc::SIGHUP, &ignored, std::ptr::null_mut()),
            0
        );
        let mut custom = ignored;
        custom.sa_sigaction = previous_handler as *const () as usize;
        assert_eq!(
            libc::sigaction(libc::SIGTERM, &custom, std::ptr::null_mut()),
            0
        );
        let before_hup = action(libc::SIGHUP);
        let before_term = action(libc::SIGTERM);
        let before_int = action(libc::SIGINT);
        let lease = SessionSignals::install().unwrap();
        assert!(SessionSignals::install().is_err());
        assert_eq!(lease.received(), None);
        for signal in [libc::SIGHUP, libc::SIGTERM] {
            let installed = action(signal);
            assert_ne!(installed.sa_flags & libc::SA_RESTART, 0);
            assert_eq!(libc::sigismember(&installed.sa_mask, libc::SIGUSR1), 0);
        }
        libc::raise(libc::SIGTERM);
        assert_eq!(lease.received(), Some(SessionSignal::Terminate));
        libc::raise(libc::SIGHUP);
        assert_eq!(lease.received(), Some(SessionSignal::Hangup));
        same_action(&before_int, &action(libc::SIGINT));
        drop(lease);
        same_action(&before_hup, &action(libc::SIGHUP));
        same_action(&before_term, &action(libc::SIGTERM));
        let next = SessionSignals::install().unwrap();
        assert_eq!(next.received(), None);
        drop(next);
        same_action(&before_hup, &action(libc::SIGHUP));
        same_action(&before_term, &action(libc::SIGTERM));
    }
}

extern "C" fn previous_handler(_: libc::c_int) {}

unsafe fn action(signal: libc::c_int) -> libc::sigaction {
    unsafe {
        let mut action = std::mem::zeroed();
        assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut action), 0);
        action
    }
}

fn same_action(expected: &libc::sigaction, actual: &libc::sigaction) {
    assert_eq!(expected.sa_sigaction, actual.sa_sigaction);
    assert_eq!(expected.sa_flags, actual.sa_flags);
    for signal in 1..=31 {
        unsafe {
            assert_eq!(
                libc::sigismember(&expected.sa_mask, signal),
                libc::sigismember(&actual.sa_mask, signal)
            );
        }
    }
}
