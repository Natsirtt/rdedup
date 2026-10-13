use rdedup_lib::backends::local::Local;
use rdedup_server::configuration::Configuration;
use rdedup_server::server::Server;

#[tokio::test]
async fn binding_does_not_disturb_existing_protection_and_shutdown_closes_listener(
) {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let configuration = directory.path().join("server.toml");
    std::fs::write(&configuration, format!(
        "repository = {}\nlisten = '127.0.0.1:0'\n[[credentials]]\nname = 'test'\ntoken = '{}'\n",
        toml::Value::String(repository.to_str().unwrap().to_owned()),
        "ab".repeat(32),
    )).unwrap();
    let storage = Local::new(repository);
    let protection = storage.exclusive_operation().unwrap();
    let server =
        Server::bind(Configuration::load(Some(&configuration)).unwrap())
            .await
            .unwrap();
    assert!(
        storage.try_shared_operation().is_err(),
        "binding must preserve an existing exclusive holder"
    );
    drop(protection);
    drop(storage.try_exclusive_operation().unwrap());
    let address = server.address();
    let (shutdown, requested) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(server.run(async {
        let _ = requested.await;
    }));
    let connection = tokio::net::TcpStream::connect(address).await.unwrap();
    drop(connection);
    shutdown.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}
