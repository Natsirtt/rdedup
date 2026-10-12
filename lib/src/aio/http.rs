//! Writable HTTP backend with operation-owned server leases.
mod error;
mod lease;
mod stream;
mod timing;
mod transport;

use super::Metadata;
use crate::backends::{
    Backend, BackendOperation, BackendThread, ChunkPromotion, Exclusive,
    OperationState, Shared,
};
pub use error::Error;
use lease::Lease;
pub use rdedup_protocol::credential::BearerToken;
use rdedup_protocol::lease::LeaseMode;
use rdedup_protocol::object::{
    DirectoryQuery, ListScope, Rename, RepositoryPath,
};
use rdedup_protocol::problem::ProblemKind;
use rdedup_protocol::route;
use reqwest::blocking::Body;
use reqwest::Method;
use sgdata::SGData;
use std::io::{self, BufReader, Read};
use std::path::PathBuf;
use std::sync::{mpsc::Sender, Arc};
use transport::{Retry, Transport};
use url::Url;

/// A connection pool and optional credential for one HTTP repository.
/// Each repository operation acquires and owns its own server lease.
pub struct Http {
    transport: Transport,
}
impl Http {
    /// Configure an HTTP repository, optionally through a TLS reverse proxy.
    ///
    /// # Errors
    /// Rejects non-HTTP URLs, embedded credentials, query strings and fragments,
    /// or failure to construct the HTTP connection pool.
    pub fn new(url: Url, token: Option<BearerToken>) -> Result<Self, Error> {
        Ok(Self {
            transport: Transport::new(url, token)?,
        })
    }
    pub(crate) fn from_environment(url: Url) -> Result<Self, Error> {
        let token = match std::env::var("RDEDUP_HTTP_TOKEN") {
            Ok(value) => Some(value.parse().map_err(|_| Error::Credential)?),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err(Error::Credential),
        };
        Self::new(url, token)
    }
    fn operation<Access>(
        &self,
        mode: LeaseMode,
    ) -> io::Result<BackendOperation<Access>> {
        let lease = Arc::new(
            Lease::acquire(self.transport.clone(), mode)
                .map_err(io::Error::from)?,
        );
        Ok(BackendOperation::new(Operation {
            transport: self.transport.clone(),
            lease,
        }))
    }
}
impl Backend for Http {
    fn begin_shared(&self) -> io::Result<BackendOperation<Shared>> {
        self.operation(LeaseMode::Shared)
    }
    fn begin_exclusive(&self) -> io::Result<BackendOperation<Exclusive>> {
        self.operation(LeaseMode::Exclusive)
    }
}
struct Operation {
    transport: Transport,
    lease: Arc<Lease>,
}
impl OperationState for Operation {
    fn new_thread(&self) -> io::Result<Box<dyn BackendThread>> {
        self.lease.check().map_err(io::Error::from)?;
        Ok(Box::new(HttpThread {
            transport: self.transport.clone(),
            lease: Arc::clone(&self.lease),
        }))
    }
}
/// A worker retaining the exact lease that admitted its repository operation.
/// Workers can only be constructed through a protected backend operation.
pub struct HttpThread {
    transport: Transport,
    lease: Arc<Lease>,
}
impl HttpThread {
    fn path(path: &std::path::Path) -> Result<RepositoryPath, Error> {
        RepositoryPath::from_path(path).map_err(|_| Error::Protocol)
    }
    fn execute(
        &self,
        method: Method,
        route: &str,
        suffix: Option<&str>,
        retry: Retry,
        mut configure: impl FnMut(
            reqwest::blocking::RequestBuilder,
        ) -> reqwest::blocking::RequestBuilder,
    ) -> Result<reqwest::blocking::Response, Error> {
        let url = self.transport.endpoint(route, suffix)?;
        self.transport.execute(retry, || {
            self.lease.check()?;
            let request = self
                .transport
                .request(method.clone(), url.clone())
                .header(route::LEASE_HEADER, self.lease.id().to_string());
            Ok(configure(request))
        })
    }
    fn enumerate(
        &self,
        path: PathBuf,
        scope: ListScope,
        mut entry: impl FnMut(PathBuf) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let path = if path.as_os_str().is_empty()
            || path == std::path::Path::new(".")
        {
            None
        } else {
            Some(Self::path(&path)?)
        };
        let query = DirectoryQuery { path, scope };
        let response = self.execute(
            Method::GET,
            route::DIRECTORIES,
            None,
            Retry::Idempotent,
            |request| request.query(&query),
        )?;
        stream::listing(BufReader::new(response), |path| {
            self.lease.check()?;
            entry(path)
        })?;
        self.lease.check()
    }
    fn delete(
        &self,
        route: &str,
        suffix: Option<&str>,
        query: Option<DirectoryQuery>,
    ) -> Result<(), Error> {
        match self.execute(
            Method::DELETE,
            route,
            suffix,
            Retry::Idempotent,
            |request| {
                if let Some(query) = &query {
                    request.query(query)
                } else {
                    request
                }
            },
        ) {
            Ok(_) | Err(Error::Problem(ProblemKind::NotFound)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}
impl BackendThread for HttpThread {
    fn promote_chunk(&mut self, promotion: ChunkPromotion) -> io::Result<()> {
        self.execute(
            Method::POST,
            route::PROMOTIONS,
            None,
            Retry::Idempotent,
            |request| request.json(&promotion),
        )
        .map(|_| ())
        .map_err(Into::into)
    }
    fn remove_dir_all(&mut self, path: PathBuf) -> io::Result<()> {
        let query = DirectoryQuery {
            path: Some(Self::path(&path).map_err(io::Error::from)?),
            scope: ListScope::Recursive,
        };
        self.delete(route::DIRECTORIES, None, Some(query))
            .map_err(Into::into)
    }
    fn rename(
        &mut self,
        source: PathBuf,
        destination: PathBuf,
    ) -> io::Result<()> {
        let rename = Rename {
            source: Self::path(&source).map_err(io::Error::from)?,
            destination: Self::path(&destination).map_err(io::Error::from)?,
        };
        self.execute(
            Method::POST,
            route::RENAMES,
            None,
            Retry::Never,
            |request| request.json(&rename),
        )
        .map(|_| ())
        .map_err(Into::into)
    }
    fn write(
        &mut self,
        path: PathBuf,
        contents: SGData,
        idempotent: bool,
    ) -> io::Result<()> {
        let path = Self::path(&path).map_err(io::Error::from)?;
        let result = self.execute(
            Method::PUT,
            route::OBJECTS,
            Some(path.as_str()),
            Retry::Idempotent,
            |request| {
                let request = if idempotent {
                    request.header(reqwest::header::IF_NONE_MATCH, "*")
                } else {
                    request
                };
                request.body(Body::sized(
                    stream::Upload::new(
                        contents.clone(),
                        Arc::clone(&self.lease),
                    ),
                    contents.len() as u64,
                ))
            },
        );
        match result {
            Ok(_) => self.lease.check().map_err(Into::into),
            // Atomic creation preserves the existing object. Name publication
            // reads it back and compares logical identity, just as with local storage.
            Err(Error::Problem(ProblemKind::AlreadyExists)) if idempotent => {
                self.lease.check().map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        }
    }
    fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        let path = Self::path(&path).map_err(io::Error::from)?;
        let mut response = self
            .execute(
                Method::GET,
                route::OBJECTS,
                Some(path.as_str()),
                Retry::Idempotent,
                |request| request,
            )
            .map_err(io::Error::from)?;
        let expected = response
            .content_length()
            .ok_or_else(|| io::Error::from(Error::Protocol))?;
        let mut contents = SGData::empty();
        loop {
            self.lease.check().map_err(io::Error::from)?;
            let mut bytes = vec![0; 128 * 1024];
            let length = response.read(&mut bytes)?;
            if length == 0 {
                break;
            }
            bytes.truncate(length);
            contents.push_vec(bytes);
        }
        self.lease.check().map_err(io::Error::from)?;
        if contents.len() as u64 != expected {
            return Err(Error::Protocol.into());
        }
        Ok(contents)
    }
    fn remove(&mut self, path: PathBuf) -> io::Result<()> {
        let path = Self::path(&path).map_err(io::Error::from)?;
        self.delete(route::OBJECTS, Some(path.as_str()), None)
            .map_err(Into::into)
    }
    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata> {
        let path = Self::path(&path).map_err(io::Error::from)?;
        let response = self
            .execute(
                Method::HEAD,
                route::OBJECTS,
                Some(path.as_str()),
                Retry::Idempotent,
                |request| request,
            )
            .map_err(io::Error::from)?;
        self.lease.check().map_err(io::Error::from)?;
        let header = response
            .headers()
            .get(route::METADATA_HEADER)
            .ok_or_else(|| io::Error::from(Error::Protocol))?;
        serde_json::from_slice(header.as_bytes())
            .map_err(|_| Error::Protocol.into())
    }
    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        self.enumerate(path, ListScope::Immediate, |path| {
            paths.push(path);
            Ok(())
        })
        .map_err(io::Error::from)?;
        Ok(paths)
    }
    fn list_recursively(
        &mut self,
        path: PathBuf,
        sender: Sender<io::Result<Vec<PathBuf>>>,
    ) {
        let mut paths = Vec::new();
        let result = self.enumerate(path, ListScope::Recursive, |path| {
            paths.push(path);
            if paths.len() == 128 {
                sender.send(Ok(std::mem::take(&mut paths))).map_err(|_| {
                    Error::Stream(io::Error::from(io::ErrorKind::BrokenPipe))
                })?;
            }
            Ok(())
        });
        match result {
            Ok(()) if !paths.is_empty() => {
                let _ = sender.send(Ok(paths));
            }
            Err(error) => {
                let _ = sender.send(Err(error.into()));
            }
            _ => {}
        }
    }
}
