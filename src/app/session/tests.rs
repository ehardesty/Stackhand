use super::*;

#[test]
fn late_cleanup_observation_recognizes_a_completed_shutdown() {
    let path = std::env::temp_dir().join(format!(
        "stackhand-completed-shutdown-{}.yaml",
        std::process::id()
    ));
    std::fs::write(
        &path,
        "version: 1\nprocesses:\n  idle:\n    command: [/bin/sh, -c, 'true']\n",
    )
    .unwrap();
    let project = crate::config::load(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let (mut session, _, _) = ProjectSession::start(project).unwrap();
    let expired = Instant::now() - SHUTDOWN_OBSERVATION_GRACE - Duration::from_secs(1);
    session
        .supervisor()
        .command(Command::Shutdown { deadline: expired });
    let shutdown = session.supervisor().snapshot().unwrap().shutdown.unwrap();
    assert!(shutdown.complete && !shutdown.timed_out);
    session.deadline = Some(expired);

    session.finish(None).unwrap();
}

#[test]
fn successful_cleanup_does_not_hide_an_application_error() {
    let error = report(
        Err(std::io::Error::from_raw_os_error(libc::EIO).into()),
        None,
        Ok(()),
    )
    .result
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(libc::EIO)
    );
}

#[test]
fn cleanup_failure_keeps_the_original_error_and_adds_context() {
    let error = report(
        Err(std::io::Error::from_raw_os_error(libc::EIO).into()),
        None,
        Err(anyhow!("Project cleanup not confirmed")),
    )
    .result
    .unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .unwrap()
            .raw_os_error(),
        Some(libc::EIO)
    );
    assert!(format!("{error:#}").contains("Project cleanup not confirmed"));
}

#[test]
fn hangup_draw_error_follows_the_cleanup_result() {
    assert!(
        report(
            Err(anyhow!("draw EIO")),
            Some(SessionSignal::Hangup),
            Ok(())
        )
        .result
        .is_ok()
    );
    let error = report(
        Err(anyhow!("draw EIO")),
        Some(SessionSignal::Hangup),
        Err(anyhow!("cleanup failed")),
    )
    .result
    .unwrap_err();
    assert_eq!(error.to_string(), "cleanup failed");
}
