//! HTTP routing uses authenticated principals and operation-owned lease permits.
use crate::authentication::Credentials;
use crate::configuration::{Configuration, ReadPolicy};
use crate::lease::{LeaseService, Principal};
use crate::response::{self, Error as ResponseError};
use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use rdedup_protocol::lease::{
    AcquireLease, Acquisition, LeaseId, LeaseMode, LeaseRequestId,
};
use rdedup_protocol::problem::ProblemKind;
use rdedup_protocol::route;
use std::future::Future;
use std::sync::Arc;

/// Server startup or listener failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The configured repository root is absent or is not an accessible directory.
    #[error("repository directory {path} is unavailable: {source}")]
    Repository {
        /// Configured repository location.
        path: std::path::PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Binding or serving the selected TCP address failed.
    #[error("HTTP listener failed")]
    Listen(#[from] std::io::Error),
}

/// A validated repository service with an owned, bound TCP listener.
pub struct Server {
    listener: tokio::net::TcpListener,
    address: std::net::SocketAddr,
    routes: Router,
}

impl Server {
    /// Validate the repository directory before binding the HTTP listener.
    ///
    /// An existing empty directory can be initialized through the HTTP backend.
    ///
    /// # Errors
    /// Returns inaccessible repository, TCP binding or listener-address failures.
    pub async fn bind(mut configuration: Configuration) -> Result<Self, Error> {
        let validate = || {
            let path = std::fs::canonicalize(&configuration.repository)?;
            std::fs::read_dir(&path)?;
            let storage = rdedup_lib::backends::local::Local::new(path.clone());
            match storage.try_shared_operation() {
                Ok(_protection) => {}
                // Startup only probes access. Each request still acquires its
                // own protection after an existing exclusive holder releases.
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
            Ok::<_, std::io::Error>(path)
        };
        configuration.repository =
            validate().map_err(|source| Error::Repository {
                path: configuration.repository.clone(),
                source,
            })?;
        let listener =
            tokio::net::TcpListener::bind(configuration.listen).await?;
        let address = listener.local_addr()?;
        Ok(Self {
            listener,
            address,
            routes: router(configuration),
        })
    }

    /// Actual bound address, including an OS-selected port when configured as zero.
    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    /// Accept requests until shutdown, then finish active connections.
    ///
    /// # Errors
    /// Returns a listener failure while serving requests.
    pub async fn run(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), Error> {
        axum::serve(self.listener, self.routes)
            .with_graceful_shutdown(shutdown)
            .await?;
        Ok(())
    }
}

pub(crate) struct Repository {
    pub(crate) leases: LeaseService,
    credentials: Credentials,
    reads: ReadPolicy,
}

impl Repository {
    pub(crate) fn lease_id(
        &self,
        headers: &HeaderMap,
    ) -> Result<LeaseId, ResponseError> {
        let mut values = headers.get_all(route::LEASE_HEADER).iter();
        let value = values
            .next()
            .ok_or(ResponseError::Problem(ProblemKind::LeaseRequired))?;
        if values.next().is_some() {
            return Err(ResponseError::Problem(ProblemKind::InvalidRequest));
        }
        value
            .to_str()
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or(ResponseError::Problem(ProblemKind::InvalidRequest))
    }

    pub(crate) fn principal(
        &self,
        headers: &HeaderMap,
    ) -> Result<Principal, ResponseError> {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let header = values.next();
        if values.next().is_some() {
            return Err(ResponseError::Problem(ProblemKind::Unauthorized));
        }
        let header = header
            .map(|value| value.to_str())
            .transpose()
            .map_err(|_| ResponseError::Problem(ProblemKind::Unauthorized))?;
        self.credentials
            .authenticate(header)
            .map_err(|_| ResponseError::Problem(ProblemKind::Unauthorized))
    }

    pub(crate) fn read_principal(
        &self,
        headers: &HeaderMap,
    ) -> Result<Principal, ResponseError> {
        let principal = self.principal(headers)?;
        if matches!(self.reads, ReadPolicy::Authenticated)
            && principal == Principal::Anonymous
        {
            return Err(ResponseError::Problem(ProblemKind::Unauthorized));
        }
        Ok(principal)
    }
}

/// Construct routes for a configured repository in the current Tokio runtime.
///
/// # Panics
/// Requires a Tokio runtime with time support for the lease coordinator.
fn router(configuration: Configuration) -> Router {
    let repository = Arc::new(Repository {
        leases: LeaseService::with_request_policy(
            configuration.repository,
            configuration.leases,
            configuration.requests,
        ),
        credentials: configuration.credentials,
        reads: configuration.reads,
    });
    Router::new()
        .route(route::PROMOTIONS, post(crate::mutation::promote))
        .route(route::RENAMES, post(crate::mutation::rename))
        .route(
            route::DIRECTORIES,
            get(crate::directory::list).delete(crate::directory::remove),
        )
        .route(
            &route::object_pattern(),
            get(crate::object::get)
                .head(crate::object::head)
                .put(crate::object::put)
                .delete(crate::object::remove),
        )
        .route(
            &route::lease_request_pattern(),
            put(acquire).get(status).delete(cancel),
        )
        .route(&route::lease_pattern(), patch(renew).delete(release))
        .fallback(response::not_found)
        .method_not_allowed_fallback(response::method_not_allowed)
        .with_state(repository)
}

/// Serve plain HTTP until shutdown. Deploy behind a trusted TLS reverse proxy.
///
/// # Errors
/// Returns repository validation, TCP bind or listener failure.
pub async fn serve(
    configuration: Configuration,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Error> {
    Server::bind(configuration).await?.run(shutdown).await
}

async fn acquire(
    State(repository): State<Arc<Repository>>,
    request: Result<Path<LeaseRequestId>, PathRejection>,
    headers: HeaderMap,
    payload: Result<Json<AcquireLease>, JsonRejection>,
) -> Result<Json<Acquisition>, ResponseError> {
    let Path(request) = request
        .map_err(|_| ResponseError::Problem(ProblemKind::InvalidRequest))?;
    let Json(payload) = payload
        .map_err(|_| ResponseError::Problem(ProblemKind::InvalidRequest))?;
    let principal = match payload.mode {
        LeaseMode::Shared => repository.read_principal(&headers)?,
        LeaseMode::Exclusive => repository.principal(&headers)?,
    };
    Ok(Json(
        repository
            .leases
            .acquire(principal, request, payload.mode)
            .await?,
    ))
}

async fn status(
    State(repository): State<Arc<Repository>>,
    request: Result<Path<LeaseRequestId>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<Acquisition>, ResponseError> {
    let Path(request) = request
        .map_err(|_| ResponseError::Problem(ProblemKind::InvalidRequest))?;
    let principal = repository.read_principal(&headers)?;
    Ok(Json(repository.leases.status(principal, request).await?))
}

async fn cancel(
    State(repository): State<Arc<Repository>>,
    request: Result<Path<LeaseRequestId>, PathRejection>,
    headers: HeaderMap,
) -> Result<StatusCode, ResponseError> {
    let Path(request) = request
        .map_err(|_| ResponseError::Problem(ProblemKind::InvalidRequest))?;
    let principal = repository.read_principal(&headers)?;
    repository.leases.cancel(principal, request).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn renew(
    State(repository): State<Arc<Repository>>,
    lease: Result<Path<LeaseId>, PathRejection>,
    headers: HeaderMap,
) -> Result<Json<Acquisition>, ResponseError> {
    let Path(lease) = lease
        .map_err(|_| ResponseError::Problem(ProblemKind::InvalidRequest))?;
    let principal = repository.read_principal(&headers)?;
    Ok(Json(repository.leases.renew(principal, lease).await?))
}

async fn release(
    State(repository): State<Arc<Repository>>,
    lease: Result<Path<LeaseId>, PathRejection>,
    headers: HeaderMap,
) -> Result<StatusCode, ResponseError> {
    let Path(lease) = lease
        .map_err(|_| ResponseError::Problem(ProblemKind::InvalidRequest))?;
    let principal = repository.read_principal(&headers)?;
    repository.leases.release(principal, lease).await?;
    Ok(StatusCode::NO_CONTENT)
}
