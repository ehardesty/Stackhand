use super::*;

#[test]
fn successful_cleanup_does_not_hide_an_application_error() {
    let error = report(
        Err(std::io::Error::from_raw_os_error(libc::EIO).into()),
        None,
        Ok(()),
    )
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
        .is_ok()
    );
    let error = report(
        Err(anyhow!("draw EIO")),
        Some(SessionSignal::Hangup),
        Err(anyhow!("cleanup failed")),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "cleanup failed");
}
