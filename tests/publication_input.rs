use std::io::Write;
use std::process::{Command, Output, Stdio};

fn successful(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn file_spooled_and_streaming_inputs_restore_identical_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rdedup"));
        command.arg("--dir").arg(&repository);
        command.env_remove("RDEDUP_DIR");
        command.env_remove("RDEDUP_URI");
        command
    };
    successful(command().arg("init").output().unwrap());
    let bytes: Vec<u8> = (0..65_536).map(|index| (index % 251) as u8).collect();
    let input = directory.path().join("input.bin");
    std::fs::write(&input, &bytes).unwrap();
    successful(
        command()
            .args(["write", "file", "--input"])
            .arg(&input)
            .output()
            .unwrap(),
    );
    for name in ["spooled", "streaming"] {
        let mut command = command();
        command.args(["store", name]);
        if name == "spooled" {
            command.arg("--spool");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        successful(child.wait_with_output().unwrap());
    }
    for name in ["file", "spooled", "streaming"] {
        let output =
            successful(command().args(["load", name]).output().unwrap());
        assert_eq!(output.stdout, bytes);
    }
}

#[test]
fn input_file_and_stdin_spooling_are_mutually_exclusive() {
    let output = Command::new(env!("CARGO_BIN_EXE_rdedup"))
        .args(["write", "name", "--input", "archive.tar", "--spool"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("--input"));
    assert!(error.contains("--spool"));
}
