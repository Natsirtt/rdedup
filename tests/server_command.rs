use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn successful(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn serve_uses_its_configuration_and_supports_remote_cli_publication() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let configuration = directory.path().join("server.toml");
    let token = "ab".repeat(32);
    std::fs::write(
        &configuration,
        format!(
            "repository = {}\nlisten = '127.0.0.1:0'\nrequire_read_credentials = true\n[[credentials]]\nname = 'test'\ntoken = '{token}'\n",
            toml::Value::String(repository.to_str().unwrap().to_owned())
        ),
    ).unwrap();
    let mut server = ServerProcess(
        Command::new(env!("CARGO_BIN_EXE_rdedup"))
            .args(["serve", "--config"])
            .arg(configuration)
            .env_remove("RDEDUP_URI")
            .env_remove("RDEDUP_DIR")
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stderr = server.0.stderr.take().unwrap();
    let (ready, startup) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(stderr).read_line(&mut line).unwrap();
        ready.send(line).unwrap();
    });
    let line = startup.recv_timeout(Duration::from_secs(15)).unwrap();
    reader.join().unwrap();
    let address = line
        .strip_prefix("Serving plain HTTP on ")
        .and_then(|remainder| remainder.split(';').next())
        .unwrap_or_else(|| panic!("server startup failed: {line}"));
    let url = format!("http://{address}");
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rdedup"));
        command
            .args(["--repo", &url])
            .env("RDEDUP_HTTP_TOKEN", &token);
        command
    };
    successful(command().arg("init").output().unwrap());
    let input = directory.path().join("archive.tar");
    let bytes = b"remote CLI publication";
    std::fs::write(&input, bytes).unwrap();
    successful(
        command()
            .args(["write", "artifact", "--input"])
            .arg(&input)
            .output()
            .unwrap(),
    );
    assert_eq!(
        successful(command().args(["load", "artifact"]).output().unwrap())
            .stdout,
        bytes
    );
    let anonymous = command()
        .env_remove("RDEDUP_HTTP_TOKEN")
        .arg("list")
        .output()
        .unwrap();
    assert!(
        !anonymous.status.success(),
        "configured private reads must require a credential"
    );
}
