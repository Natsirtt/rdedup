use super::{Backend, BackendThread, Lock, Metadata};
use reqwest::blocking::{Client, Response};
use reqwest::header::IF_NONE_MATCH;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseMode {
    Shared,
    Exclusive,
}

#[derive(Clone)]
struct LeaseRegistry {
    leases: Arc<Mutex<HashMap<Uuid, LeaseMode>>>,
}

impl LeaseRegistry {
    fn new() -> Self {
        LeaseRegistry {
            leases: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn insert(&self, lease_id: Uuid, mode: LeaseMode) {
        self.leases
            .lock()
            .expect("lease registry poisoned")
            .insert(lease_id, mode);
    }

    fn remove(&self, lease_id: Uuid) {
        self.leases
            .lock()
            .expect("lease registry poisoned")
            .remove(&lease_id);
    }

    fn lease_for(&self, required_mode: LeaseMode) -> io::Result<Uuid> {
        self.leases
            .lock()
            .expect("lease registry poisoned")
            .iter()
            .find_map(|(lease_id, mode)| {
                if required_mode == LeaseMode::Shared
                    || *mode == LeaseMode::Exclusive
                {
                    Some(*lease_id)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "no active rdedup HTTP lease",
                )
            })
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
        self.registry.insert(lease_id, mode);
        Ok(Box::new(HttpLeaseGuard::new(
            lease_id,
            mode,
            self.endpoint("leases", Some(&lease_id.to_string()))?,
            self.client.clone(),
            self.token.clone(),
            self.registry.clone(),
            lease.expires_at_unix_ms,
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
        expires_at_unix_ms: u128,
    ) -> Self {
        let (stop, stop_receiver) = mpsc::channel();
        let renewal_client = client.clone();
        let renewal_url = lease_url.clone();
        let renewal_token = token.clone();
        let renew_thread = thread::spawn(move || {
            let mut next_renewal_interval =
                renewal_interval(expires_at_unix_ms);
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
        self.registry.remove(self.lease_id);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(renew_thread) = self.renew_thread.take() {
            let _ = renew_thread.join();
        }
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
    expires_at_unix_ms: u128,
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
        let request =
            self.request(reqwest::Method::PUT, url, LeaseMode::Shared)?;
        let request = if idempotent {
            request.header(IF_NONE_MATCH, "*")
        } else {
            request
        };
        let mut body = Vec::with_capacity(sg.len());
        for part in sg.as_parts() {
            body.extend_from_slice(part);
        }
        self.response(request.body(body).send().map_err(connection_error)?)?;
        Ok(())
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

fn response_error(response: Response) -> io::Error {
    let status = response.status();
    let message = response.text().unwrap_or_default();
    let kind = match status.as_u16() {
        400 => io::ErrorKind::InvalidInput,
        401 | 403 => io::ErrorKind::PermissionDenied,
        404 => io::ErrorKind::NotFound,
        409 | 412 => io::ErrorKind::AlreadyExists,
        410 => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, format!("HTTP {status}: {message}"))
}
