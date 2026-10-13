use std::process::Command;

#[test]
fn standalone_server_exposes_configuration_without_repository_cli_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_rdedup-server"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--config"));
    assert!(!help.contains("--repo"));
}

#[test]
fn standalone_server_rejects_a_missing_repository_before_listening() {
    let directory = tempfile::tempdir().unwrap();
    let configuration = directory.path().join("server.toml");
    let repository = directory.path().join("missing");
    std::fs::write(&configuration, format!(
        "repository = {}\nlisten = '127.0.0.1:0'\n[[credentials]]\nname = 'test'\ntoken = '{}'\n",
        toml::Value::String(repository.to_str().unwrap().to_owned()),
        "ab".repeat(32),
    )).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rdedup-server"))
        .arg("--config")
        .arg(configuration)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("repository directory"));
    assert!(!error.contains("Serving"));
    assert!(
        !repository.exists(),
        "startup must not create a misspelled repository root"
    );
}
