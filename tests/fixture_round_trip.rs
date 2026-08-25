use assert_cmd::Command;

#[test]
fn executable_round_trips_input_through_a_real_pty() {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary must build");

    command
        .args(["--fixture-round-trip", "hello-stackhand"])
        .assert()
        .success()
        .stdout(predicates::str::contains("fixture-echo:hello-stackhand"))
        .stdout(predicates::str::contains("fixture-size:42x12"));
}

#[test]
fn executable_preserves_modern_terminal_render_state() {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary must build");

    command
        .arg("--fixture-rendering")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "render-fixture: colors styles unicode cursor alternate-screen reflow resize ok",
        ));
}

#[test]
fn executable_delivers_encoded_input_and_terminal_responses_to_the_child() {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary must build");

    command
        .arg("--fixture-input")
        .assert()
        .success()
        .stdout(predicates::str::contains("input-bytes:"));
}

#[test]
fn executable_handles_normal_bracketed_oversized_and_blocked_paste() {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary must build");

    command
        .arg("--fixture-paste")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "paste-fixture: normal and bracketed bytes preserved; oversized paste rejected atomically; blocked paste warning visible",
        ));
}

#[test]
fn executable_drains_output_while_scrolled_and_unfocused() {
    let mut command = Command::cargo_bin("stackhand").expect("Stackhand binary must build");

    command
        .arg("--fixture-scrollback")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "scrollback-fixture: bounded history continued draining while scrolled and unfocused",
        ));
}
