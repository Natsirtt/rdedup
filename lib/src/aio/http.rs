use super::{Backend, BackendThread, Lock, Metadata};
use reqwest::blocking::{Client, Response};
use reqwest::header::{IF_NONE_MATCH, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use sgdata::SGData;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LeaseMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy)]
struct ActiveLease {
    mode: LeaseMode,
    expires_at_unix_ms: u128,
    renewal_deadline_unix_ms: Option<u128>,
}

#[derive(Clone)]
struct LeaseRegistry {
    leases: Arc<Mutex<HashMap<Uuid, ActiveLease>>>,
}

impl LeaseRegistry {
    fn new() -> Self {
        LeaseRegistry {
            leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn insert(&self, lease_id: Uuid, lease: &LeaseResponse) {
        self.leases.lock().expect("lease registry poisoned").insert(
            lease_id,
            ActiveLease {
                mode: lease.mode,
                expires_at_unix_ms: lease.expires_at_unix_ms,
                renewal_deadline_unix_ms: lease.renewal_deadline_unix_ms,
            },
        );
    }

    fn renew(&self, lease_id: Uuid, lease: &LeaseResponse) {
        self.insert(lease_id, lease);
    }

    fn remove(&self, lease_id: Uuid) {
        self.leases
            .lock()
            .expect("lease registry poisoned")
            .remove(&lease_id);
    }

    fn lease_for(&self, required_mode: LeaseMode) -> io::Result<Uuid> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let leases = self.leases.lock().expect("lease registry poisoned");
        let mut inactive_lease_error = None;
        for (lease_id, lease) in &*leases {
            if required_mode == LeaseMode::Exclusive
                && lease.mode != LeaseMode::Exclusive
            {
                continue;
            }
            if lease
                .renewal_deadline_unix_ms
                .is_some_and(|deadline| now >= deadline)
            {
                inactive_lease_error = Some(lease_client_error(
                    409,
                    "urn:rdedup:problem:lease-renewal-closed",
                    "the server has closed renewal for this lease; retry under a new lease",
                ));
                continue;
            }
            if now >= lease.expires_at_unix_ms {
                inactive_lease_error = Some(lease_client_error(
                    410,
                    "urn:rdedup:problem:lease-expired",
                    "the server lease has expired; retry under a new lease",
                ));
                continue;
            }
            return Ok(*lease_id);
        }
        if let Some(error) = inactive_lease_error {
            Err(error)
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no active rdedup HTTP lease",
            ))
        }
    }
}

static LEASE_REGISTRIES: OnceLock<Mutex<HashMap<String, Weak<LeaseRegistry>>>> =
    OnceLock::new();

fn shared_registry(base_url: &Url, token: Option<&str>) -> Arc<LeaseRegistry> {
    let token_digest = token.map(|token| {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(token.as_bytes()))
    });
    let key = format!("{}:{}", base_url, token_digest.unwrap_or_default());
    let registries =
        LEASE_REGISTRIES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registries = registries.lock().expect("lease registries poisoned");
    if let Some(registry) = registries.get(&key).and_then(Weak::upgrade) {
        return registry;
    }
    registries.retain(|_, registry| registry.strong_count() > 0);
    let registry = Arc::new(LeaseRegistry::new());
    registries.insert(key, Arc::downgrade(&registry));
    registry
}

pub struct Http {
    base_url: Url,
    token: Option<String>,
    client: Client,
    registry: Arc<LeaseRegistry>,
}

impl Http {
    pub fn new(base_url: Url) -> Self {
        Http::with_optional_token(base_url, None)
    }

    pub fn with_token(base_url: Url, token: impl Into<String>) -> Self {
        Http::with_optional_token(base_url, Some(token.into()))
    }

    fn with_optional_token(base_url: Url, token: Option<String>) -> Self {
        let registry = shared_registry(&base_url, token.as_deref());
        Http {
            base_url,
            token,
            client: Client::new(),
            registry,
        }
    }

    fn acquire_lease(&self, mode: LeaseMode) -> io::Result<Box<dyn Lock>> {
        let request_key = Uuid::new_v4().to_string();
        let mut request = self
            .client
            .post(self.endpoint("leases", None)?)
            .header("idempotency-key", request_key)
            .json(&AcquireRequest {
                mode: match mode {
                    LeaseMode::Shared => "shared",
                    LeaseMode::Exclusive => "exclusive",
                },
            });
        request = add_authorization(request, self.token.as_deref());
        let response = request.send().map_err(connection_error)?;
        let lease = if response.status().as_u16() == 201 {
            response.json::<LeaseResponse>().map_err(invalid_response)?
        } else if response.status().as_u16() == 202 {
            let pending = response
                .json::<PendingResponse>()
                .map_err(invalid_response)?;
            self.wait_for_lease(pending.request_id)?
        } else {
            return Err(response_error(response));
        };

        let lease_id = lease.lease_id;
        self.registry.insert(lease_id, &lease);
        Ok(Box::new(HttpLeaseGuard::new(
            lease_id,
            mode,
            self.endpoint("leases", Some(&lease_id.to_string()))?,
            self.client.clone(),
            self.token.clone(),
            self.registry.clone(),
            lease,
        )))
    }

    fn wait_for_lease(&self, request_id: Uuid) -> io::Result<LeaseResponse> {
        loop {
            thread::sleep(Duration::from_secs(1));
            let mut request = self.client.get(
                self.endpoint("lease-requests", Some(&request_id.to_string()))?,
            );
            request = add_authorization(request, self.token.as_deref());
            let response = request.send().map_err(connection_error)?;
            match response.status().as_u16() {
                200 => {
                    let status = response
                        .json::<RequestStatusResponse>()
                        .map_err(invalid_response)?;
                    if status.status != "granted" {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "lease request returned an unexpected status",
                        ));
                    }
                    if let Some(lease) = status.lease {
                        return Ok(lease);
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "granted lease response is missing the lease",
                    ));
                }
                202 => continue,
                410 => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "lease request expired before it was granted",
                    ));
                }
                _ => return Err(response_error(response)),
            }
        }
    }

    fn endpoint(&self, resource: &str, path: Option<&str>) -> io::Result<Url> {
        if self.base_url.query().is_some() || self.base_url.fragment().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "rdedup HTTP backend URL cannot contain a query or fragment",
            ));
        }
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rdedup HTTP backend URL cannot be used as a base",
                )
            })?;
            segments.pop_if_empty();
            segments.push("api").push("v1").push(resource);
            if let Some(path) = path {
                for segment in path.split('/') {
                    if !segment.is_empty() {
                        segments.push(segment);
                    }
                }
            }
        }
        Ok(url)
    }
}

impl Backend for Http {
    fn lock_exclusive(&self) -> io::Result<Box<dyn Lock>> {
        self.acquire_lease(LeaseMode::Exclusive)
    }

    fn lock_shared(&self) -> io::Result<Box<dyn Lock>> {
        self.acquire_lease(LeaseMode::Shared)
    }

    fn new_thread(&self) -> io::Result<Box<dyn BackendThread>> {
        Ok(Box::new(HttpThread {
            base_url: self.base_url.clone(),
            token: self.token.clone(),
            client: self.client.clone(),
            registry: self.registry.clone(),
        }))
    }
}

struct HttpLeaseGuard {
    lease_id: Uuid,
    mode: LeaseMode,
    lease_url: Url,
    client: Client,
    token: Option<String>,
    registry: Arc<LeaseRegistry>,
    stop: Option<mpsc::Sender<()>>,
    renew_thread: Option<JoinHandle<()>>,
}

impl HttpLeaseGuard {
    fn new(
        lease_id: Uuid,
        mode: LeaseMode,
        lease_url: Url,
        client: Client,
        token: Option<String>,
        registry: Arc<LeaseRegistry>,
        lease: LeaseResponse,
    ) -> Self {
        let (stop, stop_receiver) = mpsc::channel();
        let renewal_client = client.clone();
        let renewal_url = lease_url.clone();
        let renewal_token = token.clone();
        let renewal_registry = registry.clone();
        let renewal_lease_id = lease_id;
        let renew_thread = thread::spawn(move || {
            let mut next_renewal_interval =
                renewal_interval(lease.expires_at_unix_ms);
            loop {
                match stop_receiver.recv_timeout(next_renewal_interval) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                let mut request = renewal_client
                    .patch(renewal_url.clone())
                    .timeout(Duration::from_secs(10));
                request = add_authorization(request, renewal_token.as_deref());
                let Ok(response) = request.send() else {
                    break;
                };
                if response.status().as_u16() != 200 {
                    break;
                }
                let Ok(lease) = response.json::<LeaseResponse>() else {
                    break;
                };
                if lease.lease_id != renewal_lease_id {
                    break;
                }
                renewal_registry.renew(renewal_lease_id, &lease);
                next_renewal_interval =
                    renewal_interval(lease.expires_at_unix_ms);
            }
        });
        HttpLeaseGuard {
            lease_id,
            mode,
            lease_url,
            client,
            token,
            registry,
            stop: Some(stop),
            renew_thread: Some(renew_thread),
        }
    }
}

impl Lock for HttpLeaseGuard {}

impl Drop for HttpLeaseGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(renew_thread) = self.renew_thread.take() {
            let _ = renew_thread.join();
        }
        self.registry.remove(self.lease_id);
        let mut request = self.client.delete(self.lease_url.clone());
        request = add_authorization(request, self.token.as_deref());
        let _ = request.send();
        let _ = self.mode;
    }
}

fn renewal_interval(expires_at_unix_ms: u128) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let remaining = expires_at_unix_ms.saturating_sub(now) as u64;
    Duration::from_millis((remaining / 3).max(250))
}

#[derive(Serialize)]
struct AcquireRequest {
    mode: &'static str,
}

#[derive(Deserialize)]
struct PendingResponse {
    request_id: Uuid,
}

#[derive(Deserialize)]
struct RequestStatusResponse {
    status: String,
    lease: Option<LeaseResponse>,
}

#[derive(Clone, Deserialize)]
struct LeaseResponse {
    lease_id: Uuid,
    mode: LeaseMode,
    expires_at_unix_ms: u128,
    renewal_deadline_unix_ms: Option<u128>,
}

#[derive(Deserialize)]
struct DirectoryEntry {
    name: String,
}

#[derive(Serialize)]
struct RenameRequest<'a> {
    source: &'a str,
    destination: &'a str,
}

struct HttpThread {
    base_url: Url,
    token: Option<String>,
    client: Client,
    registry: Arc<LeaseRegistry>,
}

impl HttpThread {
    fn endpoint(&self, resource: &str, path: Option<&Path>) -> io::Result<Url> {
        let mut url = self.base_url.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rdedup HTTP backend URL cannot be used as a base",
                )
            })?;
            segments.pop_if_empty();
            segments.push("api").push("v1").push(resource);
            if let Some(path) = path {
                for component in path.components() {
                    match component {
                        Component::Normal(segment) => {
                            let segment =
                                segment.to_str().ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::InvalidInput,
                                        "repository path is not UTF-8",
                                    )
                                })?;
                            segments.push(segment);
                        }
                        Component::CurDir => {}
                        _ => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "repository path must be relative and cannot contain parent components",
                            ));
                        }
                    }
                }
            }
        }
        Ok(url)
    }

    fn request(
        &self,
        method: reqwest::Method,
        url: Url,
        required_mode: LeaseMode,
    ) -> io::Result<reqwest::blocking::RequestBuilder> {
        let lease_id = self.registry.lease_for(required_mode)?;
        let request = self
            .client
            .request(method, url)
            .header("x-rdedup-lease", lease_id.to_string());
        Ok(add_authorization(request, self.token.as_deref()))
    }

    fn response(&self, response: Response) -> io::Result<Response> {
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(response_error(response))
        }
    }

    fn directory_entries(
        &self,
        path: &Path,
        recursive: bool,
    ) -> io::Result<Vec<DirectoryEntry>> {
        let url = self.endpoint("directories", Some(path))?;
        let mut request =
            self.request(reqwest::Method::GET, url, LeaseMode::Shared)?;
        if recursive {
            request = request.query(&[("recursive", "true")]);
        }
        let response =
            self.response(request.send().map_err(connection_error)?)?;
        response.json().map_err(invalid_response)
    }
}

impl BackendThread for HttpThread {
    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()> {
        let url = self.endpoint("directories", Some(&path))?;
        let request =
            self.request(reqwest::Method::DELETE, url, LeaseMode::Exclusive)?;
        self.response(request.send().map_err(connection_error)?)?;
        Ok(())
    }

    fn rename(
        &mut self,
        src_path: PathBuf,
        dst_path: PathBuf,
    ) -> io::Result<()> {
        let source = path_string(&src_path)?;
        let destination = path_string(&dst_path)?;
        let url = self.endpoint("renames", None)?;
        let request =
            self.request(reqwest::Method::POST, url, LeaseMode::Exclusive)?;
        self.response(
            request
                .json(&RenameRequest {
                    source: &source,
                    destination: &destination,
                })
                .send()
                .map_err(connection_error)?,
        )?;
        Ok(())
    }

    fn write(
        &mut self,
        path: PathBuf,
        sg: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        let url = self.endpoint("objects", Some(&path))?;
        let mut body = Vec::with_capacity(sg.len());
        for part in sg.as_parts() {
            body.extend_from_slice(part);
        }
        let maximum_attempts = if idempotent { 4 } else { 1 };
        for attempt in 0..maximum_attempts {
            let request = self.request(
                reqwest::Method::PUT,
                url.clone(),
                LeaseMode::Shared,
            )?;
            let request = if idempotent {
                request.header(IF_NONE_MATCH, "*")
            } else {
                request
            };
            match request.body(body.clone()).send() {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response)
                    if idempotent
                        && attempt + 1 < maximum_attempts
                        && retryable_status(response.status().as_u16()) =>
                {
                    let delay = retry_delay(&response, attempt);
                    drop(response);
                    thread::sleep(delay);
                }
                Ok(response) => return Err(response_error(response)),
                Err(error)
                    if idempotent
                        && attempt + 1 < maximum_attempts
                        && (error.is_timeout() || error.is_connect()) =>
                {
                    thread::sleep(backoff_delay(attempt));
                }
                Err(error) => return Err(connection_error(error)),
            }
        }
        Err(io::Error::other(
            "idempotent HTTP write retry loop ended unexpectedly",
        ))
    }

    fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        let url = self.endpoint("objects", Some(&path))?;
        let request =
            self.request(reqwest::Method::GET, url, LeaseMode::Shared)?;
        let response =
            self.response(request.send().map_err(connection_error)?)?;
        let bytes = response.bytes().map_err(connection_error)?;
        Ok(SGData::from_single(bytes.to_vec()))
    }

    fn remove(&mut self, path: PathBuf) -> io::Result<()> {
        let url = self.endpoint("objects", Some(&path))?;
        let request =
            self.request(reqwest::Method::DELETE, url, LeaseMode::Exclusive)?;
        self.response(request.send().map_err(connection_error)?)?;
        Ok(())
    }

    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata> {
        let url = self.endpoint("metadata", Some(&path))?;
        let request =
            self.request(reqwest::Method::GET, url, LeaseMode::Shared)?;
        self.response(request.send().map_err(connection_error)?)?
            .json()
            .map_err(invalid_response)
    }

    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        let entries = self.directory_entries(&path, false)?;
        Ok(entries
            .into_iter()
            .map(|entry| path.join(entry.name))
            .collect())
    }

    fn list_recursively(
        &mut self,
        path: PathBuf,
        tx: mpsc::Sender<io::Result<Vec<PathBuf>>>,
    ) {
        let result = self.directory_entries(&path, true).map(|entries| {
            entries
                .into_iter()
                .map(|entry| PathBuf::from(entry.name))
                .collect()
        });
        let _ = tx.send(result);
    }
}

fn path_string(path: &Path) -> io::Result<String> {
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                segments.push(segment.to_str().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "repository path is not UTF-8",
                    )
                })?)
            }
            Component::CurDir => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "repository path must be relative",
                ));
            }
        }
    }
    Ok(segments.join("/"))
}

fn add_authorization(
    request: reqwest::blocking::RequestBuilder,
    token: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match token {
        Some(token) => request.bearer_auth(token),
        None => request,
    }
}

fn connection_error(error: reqwest::Error) -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string())
}

fn invalid_response(error: reqwest::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 502 | 503 | 504)
}

fn backoff_delay(attempt: usize) -> Duration {
    Duration::from_millis(250u64.saturating_mul(1 << attempt.min(3)))
}

fn retry_delay(response: &Response, attempt: usize) -> Duration {
    response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| retry_after_delay(value, SystemTime::now()))
        .unwrap_or_else(|| backoff_delay(attempt))
}

fn retry_after_delay(value: &str, now: SystemTime) -> Option<Duration> {
    value
        .parse::<u64>()
        .map(Duration::from_secs)
        .ok()
        .or_else(|| {
            httpdate::parse_http_date(value).ok().map(|retry_at| {
                retry_at.duration_since(now).unwrap_or_default()
            })
        })
}

#[derive(Debug, thiserror::Error)]
pub enum HttpBackendError {
    #[error("HTTP {status} {title}: {detail} ({problem_type})")]
    Problem {
        status: u16,
        problem_type: String,
        title: String,
        detail: String,
    },
    #[error("HTTP lease expired: {detail}")]
    LeaseExpired { status: u16, detail: String },
    #[error("HTTP lease renewal closed: {detail}")]
    LeaseRenewalClosed { status: u16, detail: String },
}

impl HttpBackendError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Problem { status, .. }
            | Self::LeaseExpired { status, .. }
            | Self::LeaseRenewalClosed { status, .. } => *status,
        }
    }

    pub fn problem_type(&self) -> &str {
        match self {
            Self::Problem { problem_type, .. } => problem_type,
            Self::LeaseExpired { .. } => "urn:rdedup:problem:lease-expired",
            Self::LeaseRenewalClosed { .. } => {
                "urn:rdedup:problem:lease-renewal-closed"
            }
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::Problem { detail, .. }
            | Self::LeaseExpired { detail, .. }
            | Self::LeaseRenewalClosed { detail, .. } => detail,
        }
    }
}

fn lease_client_error(
    status: u16,
    problem_type: &str,
    detail: &str,
) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        match problem_type {
            "urn:rdedup:problem:lease-renewal-closed" => {
                HttpBackendError::LeaseRenewalClosed {
                    status,
                    detail: detail.to_owned(),
                }
            }
            _ => HttpBackendError::LeaseExpired {
                status,
                detail: detail.to_owned(),
            },
        },
    )
}

#[derive(Deserialize)]
struct ProblemDetails {
    #[serde(rename = "type")]
    problem_type: Option<String>,
    title: Option<String>,
    detail: Option<String>,
}

fn response_error(response: Response) -> io::Error {
    let status = response.status();
    let body = response.text().unwrap_or_default();
    problem_error(status.as_u16(), &body)
}

fn problem_error(status: u16, body: &str) -> io::Error {
    let problem = serde_json::from_str::<ProblemDetails>(body).ok();
    let problem_type = problem
        .as_ref()
        .and_then(|problem| problem.problem_type.clone())
        .unwrap_or_else(|| "about:blank".to_owned());
    let title = problem
        .as_ref()
        .and_then(|problem| problem.title.clone())
        .unwrap_or_else(|| {
            reqwest::StatusCode::from_u16(status)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or("HTTP error")
                .to_owned()
        });
    let detail = problem
        .as_ref()
        .and_then(|problem| problem.detail.clone())
        .unwrap_or_else(|| body.to_owned());
    let kind = match problem_type.as_str() {
        "urn:rdedup:problem:lease-expired"
        | "urn:rdedup:problem:lease-renewal-closed" => io::ErrorKind::TimedOut,
        "urn:rdedup:problem:object-conflict"
        | "urn:rdedup:problem:conflict" => io::ErrorKind::AlreadyExists,
        _ => match status {
            400 => io::ErrorKind::InvalidInput,
            401 | 403 => io::ErrorKind::PermissionDenied,
            404 => io::ErrorKind::NotFound,
            410 => io::ErrorKind::TimedOut,
            _ => io::ErrorKind::Other,
        },
    };
    let source = match problem_type.as_str() {
        "urn:rdedup:problem:lease-expired" => {
            HttpBackendError::LeaseExpired { status, detail }
        }
        "urn:rdedup:problem:lease-renewal-closed" => {
            HttpBackendError::LeaseRenewalClosed { status, detail }
        }
        _ => HttpBackendError::Problem {
            status,
            problem_type,
            title,
            detail,
        },
    };
    io::Error::new(kind, source)
}

#[cfg(test)]
mod tests {
    use super::{
        problem_error, retry_after_delay, retryable_status, HttpBackendError,
    };
    use std::io;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn server_problem_type_survives_as_typed_io_error_source() {
        let error = problem_error(
            409,
            r#"{"type":"urn:rdedup:problem:lease-renewal-closed","title":"Conflict","status":409,"detail":"renewals are closed"}"#,
        );

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let source = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<HttpBackendError>())
            .unwrap();
        assert_eq!(source.status(), 409);
        assert_eq!(
            source.problem_type(),
            "urn:rdedup:problem:lease-renewal-closed"
        );
        assert_eq!(source.detail(), "renewals are closed");
    }

    #[test]
    fn only_temporary_server_statuses_are_retryable() {
        assert!(retryable_status(408));
        assert!(retryable_status(429));
        assert!(retryable_status(503));
        assert!(!retryable_status(400));
        assert!(!retryable_status(409));
        assert!(!retryable_status(500));
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(retry_after_delay("7", now), Some(Duration::from_secs(7)));
        let date = httpdate::fmt_http_date(now + Duration::from_secs(12));
        assert_eq!(
            retry_after_delay(&date, now),
            Some(Duration::from_secs(12))
        );
    }
}
