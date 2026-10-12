use rdedup_protocol::lease::{
    AcquireLease, Acquisition, LeaseGrant, LeaseMode, LeaseRequestId,
};
use rdedup_protocol::problem::{Problem, ProblemKind};
use rdedup_protocol::route;
use rdedup_server::configuration::Configuration;
use reqwest::{Client, Response};
use std::time::Duration;
use tempfile::TempDir;
use tokio::task::JoinHandle;

struct Server {
    address: String,
    client: Client,
    task: JoinHandle<()>,
    _directory: TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn start(require_reads: bool) -> Self {
        let directory = TempDir::new().unwrap();
        let configuration_path = directory.path().join("server.toml");
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let configuration = format!(
            "repository = {}\nrequire_read_credentials = {require_reads}\n\
             listen = '127.0.0.1:0'\nlease_lifetime_secs = 300\n\
             exclusive_grace_secs = 900\nrequest_capacity = 4096\n\
             request_idle_secs = 60\nrequest_retention_secs = 600\n\
             [[credentials]]\nname = 'one'\ntoken = '{}'\n\
             [[credentials]]\nname = 'two'\ntoken = '{}'\n",
            toml::Value::String(repository.to_str().unwrap().to_owned()),
            "ab".repeat(32),
            "cd".repeat(32),
        );
        std::fs::write(&configuration_path, configuration).unwrap();
        let configuration =
            Configuration::load(Some(&configuration_path)).unwrap();
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, rdedup_server::server::router(configuration))
                .await
                .unwrap();
        });
        Self {
            address,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            task,
            _directory: directory,
        }
    }

    fn request_url(&self, request: LeaseRequestId) -> String {
        format!("{}{}/{request}", self.address, route::LEASE_REQUESTS)
    }

    async fn acquire(
        &self,
        request: LeaseRequestId,
        mode: LeaseMode,
        token: Option<&str>,
    ) -> Response {
        let mut operation = self
            .client
            .put(self.request_url(request))
            .json(&AcquireLease { mode });
        if let Some(token) = token {
            operation = operation.bearer_auth(token);
        }
        operation.send().await.unwrap()
    }

    async fn granted(
        &self,
        request: LeaseRequestId,
        token: Option<&str>,
    ) -> LeaseGrant {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut operation = self.client.get(self.request_url(request));
                if let Some(token) = token {
                    operation = operation.bearer_auth(token);
                }
                let response =
                    operation.send().await.unwrap().error_for_status().unwrap();
                match response.json::<Acquisition>().await.unwrap() {
                    Acquisition::Granted { lease } => return lease,
                    Acquisition::Queued => {
                        tokio::time::sleep(Duration::from_millis(5)).await
                    }
                    Acquisition::Closed => {
                        panic!("acquisition closed before admission")
                    }
                }
            }
        })
        .await
        .expect("HTTP lease admission timed out")
    }
}

async fn problem(response: Response, expected: ProblemKind) {
    assert_eq!(response.status(), expected.status());
    assert_eq!(
        response.headers()["content-type"],
        "application/problem+json"
    );
    assert_eq!(response.json::<Problem>().await.unwrap().kind, expected);
}

#[tokio::test]
async fn public_reads_allow_shared_leases_but_never_exclusive_leases() {
    let server = Server::start(false).await;
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Shared, None)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(server.granted(request, None).await.mode, LeaseMode::Shared);
    problem(
        server
            .acquire(LeaseRequestId::generate(), LeaseMode::Exclusive, None)
            .await,
        ProblemKind::Unauthorized,
    )
    .await;
    problem(
        server
            .acquire(
                LeaseRequestId::generate(),
                LeaseMode::Shared,
                Some("incorrect"),
            )
            .await,
        ProblemKind::Unauthorized,
    )
    .await;
}

#[tokio::test]
async fn acquisition_retries_recover_the_same_grant_and_enforce_credential_ownership(
) {
    let server = Server::start(true).await;
    let token = "ab".repeat(32);
    let request = LeaseRequestId::generate();
    problem(
        server.acquire(request, LeaseMode::Shared, None).await,
        ProblemKind::Unauthorized,
    )
    .await;
    // Discard the initial response: recovery must use the existing request identity.
    drop(
        server
            .acquire(request, LeaseMode::Shared, Some(&token))
            .await
            .error_for_status()
            .unwrap(),
    );
    let first = server.granted(request, Some(&token)).await;
    let retry = server
        .acquire(request, LeaseMode::Shared, Some(&token))
        .await
        .error_for_status()
        .unwrap()
        .json::<Acquisition>()
        .await
        .unwrap();
    let Acquisition::Granted { lease: recovered } = retry else {
        panic!("retry lost its grant")
    };
    assert_eq!(recovered.id, first.id);
    problem(
        server
            .acquire(request, LeaseMode::Exclusive, Some(&token))
            .await,
        ProblemKind::RequestConflict,
    )
    .await;
    let url = format!("{}{}/{}", server.address, route::LEASES, first.id);
    problem(
        server
            .client
            .patch(&url)
            .bearer_auth("cd".repeat(32))
            .send()
            .await
            .unwrap(),
        ProblemKind::Forbidden,
    )
    .await;
    let renewed = server
        .client
        .patch(&url)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Acquisition>()
        .await
        .unwrap();
    let Acquisition::Granted { lease } = renewed else {
        panic!("renewal lost its grant")
    };
    assert_eq!(lease.id, first.id);
    assert!(lease.remaining > Duration::from_secs(290));
    assert_eq!(
        server
            .client
            .delete(&url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    problem(
        server
            .client
            .patch(url)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap(),
        ProblemKind::LeaseExpired,
    )
    .await;
}

#[tokio::test]
async fn malformed_http_inputs_return_stable_problem_bodies() {
    let server = Server::start(false).await;
    let url = server.request_url(LeaseRequestId::generate());
    problem(
        server
            .client
            .put(&url)
            .header("content-type", "application/json")
            .body("{broken")
            .send()
            .await
            .unwrap(),
        ProblemKind::InvalidRequest,
    )
    .await;
    problem(
        server
            .client
            .put(&url)
            .json(&AcquireLease {
                mode: LeaseMode::Shared,
            })
            .header("authorization", format!("Bearer {}", "ab".repeat(32)))
            .header("authorization", format!("Bearer {}", "cd".repeat(32)))
            .send()
            .await
            .unwrap(),
        ProblemKind::Unauthorized,
    )
    .await;
    problem(
        server
            .client
            .get(format!(
                "{}{}/invalid",
                server.address,
                route::LEASE_REQUESTS
            ))
            .send()
            .await
            .unwrap(),
        ProblemKind::InvalidRequest,
    )
    .await;
    problem(
        server
            .client
            .get(format!("{}/absent", server.address))
            .send()
            .await
            .unwrap(),
        ProblemKind::NotFound,
    )
    .await;
}

#[tokio::test]
async fn shared_uploads_are_atomic_and_destructive_requests_require_exclusive_protection(
) {
    let server = Server::start(false).await;
    let token = "ab".repeat(32);
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Shared, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let lease = server.granted(request, Some(&token)).await;
    let path = "0000000000000000-0000000000000000/name/archive.yml";
    let url = format!("{}{}/{path}", server.address, route::OBJECTS);
    let response = server
        .client
        .put(&url)
        .bearer_auth(&token)
        .header(route::LEASE_HEADER, lease.id.to_string())
        .header("if-none-match", "*")
        .body("published bytes")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    problem(
        server
            .client
            .put(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .header("if-none-match", "*")
            .body("different bytes")
            .send()
            .await
            .unwrap(),
        ProblemKind::AlreadyExists,
    )
    .await;
    let metadata = server
        .client
        .head(&url)
        .bearer_auth(&token)
        .header(route::LEASE_HEADER, lease.id.to_string())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let metadata: rdedup_lib::backends::Metadata = serde_json::from_str(
        metadata.headers()[route::METADATA_HEADER].to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(metadata.len, 15);
    let rejected = server.client.head(&url).send().await.unwrap();
    assert_eq!(rejected.status(), ProblemKind::LeaseRequired.status());
    let details: Problem = serde_json::from_str(
        rejected.headers()[route::PROBLEM_HEADER].to_str().unwrap(),
    )
    .unwrap();
    assert_eq!(details.kind, ProblemKind::LeaseRequired);
    assert!(rejected.bytes().await.unwrap().is_empty());
    let response = server
        .client
        .get(&url)
        .bearer_auth(&token)
        .header(route::LEASE_HEADER, lease.id.to_string())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "published bytes");
    problem(
        server
            .client
            .put(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .body("destructive replacement")
            .send()
            .await
            .unwrap(),
        ProblemKind::LeaseMode,
    )
    .await;
    problem(
        server
            .client
            .delete(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .send()
            .await
            .unwrap(),
        ProblemKind::LeaseMode,
    )
    .await;
    problem(
        server.client.get(&url).send().await.unwrap(),
        ProblemKind::LeaseRequired,
    )
    .await;
    for path in [
        ".lock",
        "0000000000000000-0000000000000000/name/archive.random.tmp",
    ] {
        problem(
            server
                .client
                .get(format!("{}{}/{path}", server.address, route::OBJECTS))
                .header(route::LEASE_HEADER, lease.id.to_string())
                .bearer_auth(&token)
                .send()
                .await
                .unwrap(),
            ProblemKind::InvalidRequest,
        )
        .await;
    }
    server
        .client
        .delete(format!("{}{}/{}", server.address, route::LEASES, lease.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let exclusive = LeaseRequestId::generate();
    server
        .acquire(exclusive, LeaseMode::Exclusive, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let exclusive = server.granted(exclusive, Some(&token)).await;
    assert_eq!(
        server
            .client
            .put(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, exclusive.id.to_string())
            .body("replacement")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        server
            .client
            .get(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, exclusive.id.to_string())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap(),
        "replacement"
    );
    assert_eq!(
        server
            .client
            .delete(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, exclusive.id.to_string())
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    problem(
        server
            .client
            .get(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, exclusive.id.to_string())
            .send()
            .await
            .unwrap(),
        ProblemKind::NotFound,
    )
    .await;
}

#[tokio::test]
async fn a_stalled_upload_loses_its_lease_without_publishing_and_releases_storage(
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = Server::start(false).await;
    let token = "ab".repeat(32);
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Shared, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let lease = server.granted(request, Some(&token)).await;
    let path = "0000000000000000-0000000000000000/name/incomplete.yml";
    let mut connection = tokio::net::TcpStream::connect(
        server.address.strip_prefix("http://").unwrap(),
    )
    .await
    .unwrap();
    connection.write_all(format!("PUT {}/{path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n{}: {}\r\nIf-None-Match: *\r\nContent-Length: 100\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n", route::OBJECTS, route::LEASE_HEADER, lease.id).as_bytes()).await.unwrap();
    let mut interim = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !interim.ends_with(b"\r\n\r\n") {
            interim.push(connection.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8(interim)
        .unwrap()
        .starts_with("HTTP/1.1 100 Continue"));
    connection.write_all(b"partial").await.unwrap();
    server
        .client
        .delete(format!("{}{}/{}", server.address, route::LEASES, lease.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        connection.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 410 Gone"), "{response}");
    assert!(response.contains("urn:rdedup:problem:lease-expired"));
    assert!(!server
        ._directory
        .path()
        .join("repository")
        .join(path)
        .exists());
    let exclusive = LeaseRequestId::generate();
    server
        .acquire(exclusive, LeaseMode::Exclusive, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        server.granted(exclusive, Some(&token)).await.mode,
        LeaseMode::Exclusive
    );
}

#[tokio::test]
async fn a_stalled_download_releases_storage_on_lease_loss_without_client_reads(
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = Server::start(false).await;
    let token = "ab".repeat(32);
    let path = format!(
        "0000000000000000-0000000000000000/chunk/{}",
        "ab".repeat(32)
    );
    let native_path = server._directory.path().join("repository").join(&path);
    std::fs::create_dir_all(native_path.parent().unwrap()).unwrap();
    std::fs::File::create(native_path)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Shared, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let lease = server.granted(request, Some(&token)).await;
    let mut connection = tokio::net::TcpStream::connect(
        server.address.strip_prefix("http://").unwrap(),
    )
    .await
    .unwrap();
    connection.write_all(format!("GET {}/{path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n{}: {}\r\n\r\n", route::OBJECTS, route::LEASE_HEADER, lease.id).as_bytes()).await.unwrap();
    let mut headers = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(connection.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8(headers)
        .unwrap()
        .starts_with("HTTP/1.1 200 OK"));
    server
        .client
        .delete(format!("{}{}/{}", server.address, route::LEASES, lease.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let exclusive = LeaseRequestId::generate();
    server
        .acquire(exclusive, LeaseMode::Exclusive, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        server.granted(exclusive, Some(&token)).await.mode,
        LeaseMode::Exclusive
    );
    drop(connection);
}

#[tokio::test]
async fn directory_streams_filter_internal_entries_and_require_completion() {
    use rdedup_protocol::object::{DirectoryQuery, ListScope, ListingRecord};
    use std::collections::BTreeSet;
    let server = Server::start(false).await;
    let token = "ab".repeat(32);
    let root = server._directory.path().join("repository");
    let generation = "0000000000000000-0000000000000000";
    let name = format!("{generation}/name/archive.yml");
    let chunk = format!("{generation}/chunk/{}", "ab".repeat(32));
    let generation_configuration = format!("{generation}/config.yml");
    for path in ["config.yml", &generation_configuration, &name, &chunk] {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"stored").unwrap();
    }
    std::fs::write(
        root.join(format!("{generation}/name/upload.random.tmp")),
        b"staging",
    )
    .unwrap();
    std::fs::write(root.join("unrelated"), b"not published").unwrap();
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Shared, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let lease = server.granted(request, Some(&token)).await;
    let url = format!("{}{}", server.address, route::DIRECTORIES);
    for scope in [ListScope::Immediate, ListScope::Recursive] {
        let response = server
            .client
            .get(&url)
            .query(&DirectoryQuery { path: None, scope })
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(
            response.headers()["content-type"],
            route::LISTING_CONTENT_TYPE
        );
        let text = response.text().await.unwrap();
        let mut records = text
            .lines()
            .map(|line| serde_json::from_str::<ListingRecord>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(records.pop(), Some(ListingRecord::Complete)));
        let paths = records
            .into_iter()
            .map(|record| match record {
                ListingRecord::Entry { path } => path.as_str().to_owned(),
                other => panic!("unexpected listing record: {other:?}"),
            })
            .collect::<BTreeSet<_>>();
        let expected = match scope {
            ListScope::Immediate => {
                BTreeSet::from(["config.yml".to_owned(), generation.to_owned()])
            }
            ListScope::Recursive => BTreeSet::from([
                "config.yml".to_owned(),
                generation_configuration.clone(),
                name.clone(),
                chunk.clone(),
            ]),
        };
        assert_eq!(paths, expected);
    }
    problem(
        server
            .client
            .delete(&url)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .send()
            .await
            .unwrap(),
        ProblemKind::InvalidRequest,
    )
    .await;
    let query = DirectoryQuery {
        path: Some(generation.parse().unwrap()),
        scope: ListScope::Immediate,
    };
    problem(
        server
            .client
            .delete(&url)
            .query(&query)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .send()
            .await
            .unwrap(),
        ProblemKind::LeaseMode,
    )
    .await;
    server
        .client
        .delete(format!("{}{}/{}", server.address, route::LEASES, lease.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Exclusive, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let exclusive = server.granted(request, Some(&token)).await;
    assert_eq!(
        server
            .client
            .delete(&url)
            .query(&query)
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, exclusive.id.to_string())
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(!root.join(generation).exists());
    assert!(root.join("config.yml").is_file());
}

#[tokio::test]
async fn chunk_promotion_is_additive_and_rename_remains_exclusive() {
    use rdedup_lib::backends::ChunkPromotion;
    use rdedup_protocol::object::Rename;
    let server = Server::start(false).await;
    let token = "ab".repeat(32);
    let root = server._directory.path().join("repository");
    let old = "0000000000000000-0000000000000000";
    let new = "0000000000000001-0000000000000000";
    let source = format!("{old}/chunk/{}", "ab".repeat(32));
    let destination = format!("{new}/chunk/{}", "ab".repeat(32));
    std::fs::create_dir_all(root.join(&source).parent().unwrap()).unwrap();
    std::fs::write(root.join(&source), b"chunk bytes").unwrap();
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Shared, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let lease = server.granted(request, Some(&token)).await;
    let promotion = ChunkPromotion::from_paths(
        source.clone().into(),
        destination.clone().into(),
    )
    .unwrap();
    for _ in 0..2 {
        assert_eq!(
            server
                .client
                .post(format!("{}{}", server.address, route::PROMOTIONS))
                .bearer_auth(&token)
                .header(route::LEASE_HEADER, lease.id.to_string())
                .json(&promotion)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
    }
    assert_eq!(std::fs::read(root.join(&source)).unwrap(), b"chunk bytes");
    assert_eq!(
        std::fs::read(root.join(&destination)).unwrap(),
        b"chunk bytes"
    );
    let rename = Rename {
        source: source.parse().unwrap(),
        destination: destination.parse().unwrap(),
    };
    problem(
        server
            .client
            .post(format!("{}{}", server.address, route::RENAMES))
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .json(&rename)
            .send()
            .await
            .unwrap(),
        ProblemKind::LeaseMode,
    )
    .await;
    problem(server.client.post(format!("{}{}", server.address, route::PROMOTIONS))
        .bearer_auth(&token).header(route::LEASE_HEADER, lease.id.to_string())
        .json(&serde_json::json!({"source": destination, "destination": source}))
        .send().await.unwrap(), ProblemKind::InvalidRequest).await;
    server
        .client
        .delete(format!("{}{}/{}", server.address, route::LEASES, lease.id))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let request = LeaseRequestId::generate();
    server
        .acquire(request, LeaseMode::Exclusive, Some(&token))
        .await
        .error_for_status()
        .unwrap();
    let lease = server.granted(request, Some(&token)).await;
    let source_name = format!("{old}/name/published.yml");
    let destination_name = format!("{new}/name/published.yml");
    std::fs::create_dir_all(root.join(&source_name).parent().unwrap()).unwrap();
    std::fs::write(root.join(&source_name), b"name bytes").unwrap();
    let rename = Rename {
        source: source_name.parse().unwrap(),
        destination: destination_name.parse().unwrap(),
    };
    assert_eq!(
        server
            .client
            .post(format!("{}{}", server.address, route::RENAMES))
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .json(&rename)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(!root.join(&source_name).exists());
    assert_eq!(
        std::fs::read(root.join(&destination_name)).unwrap(),
        b"name bytes"
    );
    let staging = format!("{old}/name/unpublished.random.tmp");
    std::fs::write(root.join(&staging), b"partial").unwrap();
    let rename = Rename {
        source: staging.parse().unwrap(),
        destination: format!("{new}/name/invalid.yml").parse().unwrap(),
    };
    problem(
        server
            .client
            .post(format!("{}{}", server.address, route::RENAMES))
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .json(&rename)
            .send()
            .await
            .unwrap(),
        ProblemKind::InvalidRequest,
    )
    .await;
    assert!(root.join(staging).is_file());
    let directory_source = format!("{old}/name/folder");
    let directory_destination = format!("{new}/name/folder");
    std::fs::create_dir_all(root.join(&directory_source)).unwrap();
    std::fs::write(
        root.join(&directory_source).join("nested.yml"),
        b"nested name",
    )
    .unwrap();
    let rename = Rename {
        source: directory_source.parse().unwrap(),
        destination: directory_destination.parse().unwrap(),
    };
    assert_eq!(
        server
            .client
            .post(format!("{}{}", server.address, route::RENAMES))
            .bearer_auth(&token)
            .header(route::LEASE_HEADER, lease.id.to_string())
            .json(&rename)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert!(!root.join(directory_source).exists());
    assert_eq!(
        std::fs::read(root.join(directory_destination).join("nested.yml"))
            .unwrap(),
        b"nested name"
    );
}

#[tokio::test]
async fn backend_traits_round_trip_objects_and_retain_worker_protection() {
    use rdedup_lib::backends::{http::Http, Backend};
    use std::io::ErrorKind;
    let server = Server::start(false).await;
    let address = server.address.clone();
    tokio::task::spawn_blocking(move || {
        let token = "ab".repeat(32).parse().unwrap();
        let backend = Http::new(address.parse().unwrap(), Some(token)).unwrap();
        let operation = backend.begin_shared().unwrap();
        let mut worker = operation.new_thread().unwrap();
        drop(operation);
        worker
            .create(
                "0000000000000000-0000000000000000/name/client.yml".into(),
                sgdata::SGData::from_single(b"original".to_vec()),
            )
            .unwrap();
        worker
            .create(
                "0000000000000000-0000000000000000/name/client.yml".into(),
                sgdata::SGData::from_single(b"replacement".to_vec()),
            )
            .unwrap();
        assert_eq!(
            worker
                .read(
                    "0000000000000000-0000000000000000/name/client.yml".into()
                )
                .unwrap()
                .into_linear_vec(),
            b"original"
        );
        assert_eq!(
            worker
                .read_metadata(
                    "0000000000000000-0000000000000000/name/client.yml".into()
                )
                .unwrap()
                .len,
            8
        );
        assert_eq!(
            worker
                .read_metadata(
                    "0000000000000000-0000000000000000/name/absent.yml".into()
                )
                .unwrap_err()
                .kind(),
            ErrorKind::NotFound
        );
        assert_eq!(
            worker
                .list("0000000000000000-0000000000000000/name".into())
                .unwrap(),
            [std::path::PathBuf::from(
                "0000000000000000-0000000000000000/name/client.yml"
            )]
        );
        let (sender, receiver) = std::sync::mpsc::channel();
        worker.list_recursively(
            "0000000000000000-0000000000000000".into(),
            sender,
        );
        let paths: Vec<_> =
            receiver.into_iter().flat_map(Result::unwrap).collect();
        assert_eq!(
            paths,
            [std::path::PathBuf::from(
                "0000000000000000-0000000000000000/name/client.yml"
            )]
        );
        drop(worker);
        let exclusive = backend.begin_exclusive().unwrap();
        let mut worker = exclusive.new_thread().unwrap();
        worker
            .replace(
                "0000000000000000-0000000000000000/name/client.yml".into(),
                sgdata::SGData::from_single(b"replaced".to_vec()),
            )
            .unwrap();
        worker
            .rename(
                "0000000000000000-0000000000000000/name/client.yml".into(),
                "0000000000000000-0000000000000000/name/moved.yml".into(),
            )
            .unwrap();
        assert_eq!(
            worker
                .read("0000000000000000-0000000000000000/name/moved.yml".into())
                .unwrap()
                .into_linear_vec(),
            b"replaced"
        );
        worker
            .remove("0000000000000000-0000000000000000/name/moved.yml".into())
            .unwrap();
        worker
            .remove("0000000000000000-0000000000000000/name/moved.yml".into())
            .unwrap();
        worker
            .remove_dir_all("0000000000000000-0000000000000000".into())
            .unwrap();
    })
    .await
    .unwrap();
}
