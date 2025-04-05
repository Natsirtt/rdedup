use std::{io, mem};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use serde::Deserialize;
use url::Url;
use sgdata::SGData;
use crate::aio::Metadata;
use crate::backends::{Backend, BackendThread, Lock};

// TODO: This is meant to be a "read-only" http backend that can get data from a remote repository,
// ideally served very efficiently with a static web server (for instance, rust's static-web-server
// crate/binary).
// Therefore a mechanism to implement a file lock is not really possible in such an architecture...
// For now we ignore it because in practice only GC and rm operations need an exclusive lock (which
// waits for all exclusive locks to have expired); so we can get quite far running like this as long as
// we are careful. Eventually I would like to find a solution of sorts.
struct NoLock {}

impl Lock for NoLock {}

#[derive(Deserialize)]
struct FileInfo {
    name: String,
    mtime: String,
    size: Option<u64>,
    #[serde(rename = "type")]
    file_type: String,
}

pub struct HttpReadOnly {
    base_url: Url,
}

impl HttpReadOnly {
    pub fn new(base_url: Url) -> Self {
        HttpReadOnly { base_url }
    }
}

pub struct HttpReadOnlyThread {
    base_url: Url,
}

impl HttpReadOnlyThread {
    pub fn new(base_url: Url) -> Self {
        HttpReadOnlyThread {
            base_url,
        }
    }

    fn get_endpoint(&self, path: PathBuf) -> Result<Url, io::Error> {
        self.base_url.join(path.to_str().unwrap()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Failed to build endpoint: {}", e))
        })
    }

    fn get_blocking(&self, path: PathBuf) -> io::Result<reqwest::blocking::Response> {
        reqwest::blocking::get(self.get_endpoint(path)?).map_err(|e| {
            io::Error::new(io::ErrorKind::ConnectionAborted, format!("Request failed: {:?}", e))
        })
    }
}

impl Backend for HttpReadOnly {
    fn lock_exclusive(&self) -> io::Result<Box<dyn Lock>> {
        Err(io::Error::new(io::ErrorKind::ReadOnlyFilesystem, "Static HTTP endpoint is read-only"))
    }

    fn lock_shared(&self) -> io::Result<Box<dyn Lock>> {
        Ok(Box::new(NoLock{}))
    }

    fn new_thread(&self) -> io::Result<Box<dyn BackendThread>> {
       Ok(Box::new(HttpReadOnlyThread::new(self.base_url.clone())))
    }
}

fn get_content_type(response: &reqwest::blocking::Response) -> io::Result<&reqwest::header::HeaderValue> {
    response.headers().get(reqwest::header::CONTENT_TYPE).ok_or(io::Error::new(io::ErrorKind::InvalidData, "No content type header"))
}

fn is_file_content_type(header: &reqwest::header::HeaderValue) -> bool {
    header == "application/octet-stream" || header == "text/x-yaml"
}

fn is_directory_content_type(header: &reqwest::header::HeaderValue) -> bool {
    header == "application/json"
}

impl BackendThread for HttpReadOnlyThread {
    fn remove_dir_all(&mut self, _path: PathBuf) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::ReadOnlyFilesystem, "Static HTTP endpoint is read-only"))
    }

    fn rename(&mut self, _src_path: PathBuf, _dst_path: PathBuf) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::ReadOnlyFilesystem, "Static HTTP endpoint is read-only"))
    }

    fn write(&mut self, _path: PathBuf, _sg: SGData, _idempotent: bool) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::ReadOnlyFilesystem, "Static HTTP endpoint is read-only"))
    }

    fn read(&mut self, path: PathBuf) -> io::Result<SGData> {
        let response = self.get_blocking(path)?;
        let content_type = get_content_type(&response)?;
        if !is_file_content_type(&content_type) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Invalid content type {}", content_type.to_str().unwrap())));
        }

        let data = response.bytes().map_err(|e| {
            io::Error::new(io::ErrorKind::ConnectionAborted, format!("Failed to read response as bytes: {}", e))
        })?;

        Ok(SGData::from_single(data.into()))
    }

    fn remove(&mut self, _path: PathBuf) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::ReadOnlyFilesystem, "Static HTTP endpoint is read-only"))
    }

    fn read_metadata(&mut self, path: PathBuf) -> io::Result<Metadata> {
        let as_path = path.as_path();
        let filename = as_path.file_name().unwrap().to_str().unwrap().to_string();
        let response = self.get_blocking(PathBuf::from(as_path.parent().unwrap()))?;
        let content_type = get_content_type(&response)?;

        if !is_directory_content_type(content_type) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Invalid content type '{:?}'", content_type.to_str().unwrap())));
        }

        let file_list: Vec<FileInfo> = response.json().map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse response as JSON: {:?}", e))
        })?;
        let file: &FileInfo = file_list.iter().find(|f| f.name == filename).ok_or(io::Error::new(io::ErrorKind::NotFound, "File not found"))?;
        if !file.file_type.eq("file") {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is not a file", filename)))
        }
        let m_datetime = chrono::DateTime::parse_from_rfc3339(file.mtime.as_str()).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse mtime to a datetime: {:?}", e))
        })?;
        Ok(Metadata {
            len: file.size.expect("This is a file, so it should have a size."),
            is_file: true,
            created: m_datetime.into()
        })
    }

    fn list(&mut self, path: PathBuf) -> io::Result<Vec<PathBuf>> {
        let response = self.get_blocking(path.clone())?;
        if !response.status().is_success() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Bad response code: {}", response.status())));
        }
        let content_type = get_content_type(&response)?;
        if !is_directory_content_type(content_type) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Invalid content type {}", content_type.to_str().unwrap())));
        }
        let file_list: Vec<FileInfo> = response.json().map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("Failed to parse response as JSON: {:?}", e))
        })?;
        let mut result = Vec::with_capacity(128);
        for file in file_list {
            result.push(path.join(file.name));
        }
        Ok(result)
    }

    fn list_recursively(&mut self, path: PathBuf, tx: Sender<io::Result<Vec<PathBuf>>>) {
        let mut to_process = vec![path];
        let mut result = Vec::with_capacity(128);
        while let Some(path) = to_process.pop() {
            let response = self.get_blocking(path.clone()).unwrap();
            if !response.status().is_success() {
                tx.send(Err(io::Error::new(io::ErrorKind::InvalidData, format!("Bad response code: {}", response.status())))).expect("Send failed");
                continue;
            }
            let content_type = get_content_type(&response).unwrap();
            if !is_directory_content_type(content_type) {
                tx.send(Err(io::Error::new(io::ErrorKind::InvalidData, format!("Invalid content type {}", content_type.to_str().unwrap())))).expect("Send failed");
                continue;
            }

            let file_list: Vec<FileInfo> = response.json().unwrap_or_default();
            for file in file_list {
                let file_path = path.join(file.name);
                if file.file_type == "file" {
                    result.push(file_path);
                } else {
                    to_process.push(file_path)
                }
            }

            if result.len() > 100 {
                tx.send(Ok(mem::take(&mut result))).expect("Send failed");
            }
        }
        if !result.is_empty() {
            tx.send(Ok(result)).expect("Send failed");
        }
    }
}

