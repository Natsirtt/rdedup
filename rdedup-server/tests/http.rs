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
