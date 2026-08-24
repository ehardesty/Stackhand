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
