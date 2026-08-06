use std::process::Command;

#[test]
fn prints_exact_greeting() {
    let output = Command::new(env!("CARGO_BIN_EXE_harkness"))
        .output()
        .expect("harkness should start");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"Hello World\n");
    assert!(output.stderr.is_empty());
}
