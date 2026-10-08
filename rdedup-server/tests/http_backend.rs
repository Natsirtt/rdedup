use rdedup_lib::backends::http::Http;
use rdedup_lib::backends::Backend;
use rdedup_lib::settings;
use rdedup_lib::Repo;
use sha2::{Digest, Sha256};
use std::io::{self, Cursor};
use std::path::PathBuf;
use std::sync::Arc;
use url::Url;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

struct RepositoryDirectory(PathBuf);

impl Drop for RepositoryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn rdedup_http_backend_roundtrips_and_retries_names() {
    let repository_path = std::env::temp_dir()
        .join(format!("rdedup-http-test-{}", uuid::Uuid::new_v4()));
    let _repository_directory = RepositoryDirectory(repository_path.clone());

    let mut settings = settings::Repo::new();
    settings
        .set_compression(settings::Compression::None)
        .unwrap();
    drop(
        Repo::init_from_url(
            Arc::new(Url::from_file_path(&repository_path).unwrap()),
            &|| Ok(String::new()),
            settings,
            None,
        )
        .unwrap(),
    );

    let mut tokens = std::collections::HashMap::new();
    tokens.insert("build-agent".to_owned(), TOKEN.to_owned());
    let config = rdedup_server::config::ServerConfig {
        repository_path,
        bind_address: "127.0.0.1:0".parse().unwrap(),
        lease_ttl: std::time::Duration::from_secs(30),
        lease_renewal_grace: std::time::Duration::from_secs(60),
        lease_request_ttl: std::time::Duration::from_secs(120),
        require_read_auth: false,
        tokens,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        Url::parse(&format!("http://{}", listener.local_addr().unwrap()))
            .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, rdedup_server::router(config))
            .await
            .unwrap();
    });

    let contents = (0..512 * 1024)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let backend_endpoint = endpoint.clone();
    let public_endpoint = endpoint.clone();
    tokio::task::spawn_blocking(move || {
        let backend: Arc<rdedup_lib::BackendSelectFn> = Arc::new(move || {
            Ok(Box::new(Http::with_token(backend_endpoint.clone(), TOKEN))
                as Box<dyn Backend + Send + Sync>)
        });
        let repository = Repo::open(backend, None).unwrap();
        let encryption =
            repository.unlock_encrypt(&|| Ok(String::new())).unwrap();

        repository
            .write("artifact", Cursor::new(contents.clone()), &encryption)
            .unwrap();
        repository
            .write("artifact", Cursor::new(contents.clone()), &encryption)
            .expect("a retry with identical contents should succeed");

        let error = repository
            .write(
                "artifact",
                Cursor::new(b"different publication"),
                &encryption,
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        let decryption =
            repository.unlock_decrypt(&|| Ok(String::new())).unwrap();
        let mut downloaded = Vec::new();
        repository
            .read("artifact", &mut downloaded, &decryption)
            .unwrap();
        assert_eq!(Sha256::digest(&contents), Sha256::digest(&downloaded));
        assert_eq!(repository.list_names().unwrap(), vec!["artifact"]);

        let public_backend: Arc<rdedup_lib::BackendSelectFn> =
            Arc::new(move || {
                Ok(Box::new(Http::new(public_endpoint.clone()))
                    as Box<dyn Backend + Send + Sync>)
            });
        let public_repository = Repo::open(public_backend, None).unwrap();
        let public_decryption = public_repository
            .unlock_decrypt(&|| Ok(String::new()))
            .unwrap();
        let mut public_download = Vec::new();
        public_repository
            .read("artifact", &mut public_download, &public_decryption)
            .unwrap();
        assert_eq!(public_download, contents);
        drop(public_repository);

        repository.rm("artifact").unwrap();
        repository.gc(0).unwrap();
        assert!(repository.list_names().unwrap().is_empty());
        drop(repository);
    })
    .await
    .unwrap();

    let client = reqwest::Client::new();
    let shared_lease = client
        .post(endpoint.join("api/v1/leases").unwrap())
        .header("idempotency-key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({ "mode": "shared" }))
        .send()
        .await
        .unwrap();
    assert_eq!(shared_lease.status(), reqwest::StatusCode::CREATED);
    let lease_id = shared_lease.json::<serde_json::Value>().await.unwrap()
        ["lease_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let unauthorized_write = client
        .put(endpoint.join("api/v1/objects/auth-probe").unwrap())
        .header("x-rdedup-lease", lease_id.clone())
        .body("not authorized")
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthorized_write.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let unauthorized_exclusive_lease = client
        .post(endpoint.join("api/v1/leases").unwrap())
        .header("idempotency-key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({ "mode": "exclusive" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthorized_exclusive_lease.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    client
        .delete(endpoint.join(&format!("api/v1/leases/{lease_id}")).unwrap())
        .send()
        .await
        .unwrap();

    server.abort();
}

#[tokio::test]
async fn configured_read_auth_rejects_anonymous_reads() {
    let repository_path = std::env::temp_dir()
        .join(format!("rdedup-http-auth-test-{}", uuid::Uuid::new_v4()));
    let _repository_directory = RepositoryDirectory(repository_path.clone());
    let config = rdedup_server::config::ServerConfig {
        repository_path,
        bind_address: "127.0.0.1:0".parse().unwrap(),
        lease_ttl: std::time::Duration::from_secs(30),
        lease_renewal_grace: std::time::Duration::from_secs(60),
        lease_request_ttl: std::time::Duration::from_secs(120),
        require_read_auth: true,
        tokens: std::collections::HashMap::new(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        Url::parse(&format!("http://{}", listener.local_addr().unwrap()))
            .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, rdedup_server::router(config))
            .await
            .unwrap();
    });

    let response =
        reqwest::get(endpoint.join("api/v1/objects/config.yml").unwrap())
            .await
            .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    server.abort();
}
