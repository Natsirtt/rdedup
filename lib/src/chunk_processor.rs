use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::{error, fmt};

use sgdata::SGData;
use slog::{trace, Level, Logger};
use slog_perf::TimeReporter;

use super::aio;
use super::{DataType, Repo};
use crate::compression::ArcCompression;
use crate::encryption::ArcEncrypter;
use crate::hashing::ArcHasher;
use crate::{Digest, Generation};

pub(crate) struct Message {
    pub data: (u64, SGData),
    pub data_type: DataType,
    pub response_tx: mpsc::Sender<(u64, Digest)>,
    pub write_failure_tx: mpsc::Sender<ChunkWriteFailure>,
    pub abort_pipeline: Arc<AtomicBool>,
}

#[derive(Debug)]
pub(crate) struct ChunkWriteFailure {
    pub path: PathBuf,
    pub source: io::Error,
}

impl fmt::Display for ChunkWriteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "failed to write chunk {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl error::Error for ChunkWriteFailure {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) struct ChunkProcessor {
    repo: Repo,
    rx: crossbeam_channel::Receiver<Message>,
    aio: aio::AsyncIO,
    log: Logger,
    encrypter: ArcEncrypter,
    compressor: ArcCompression,
    hasher: ArcHasher,
    generations: Vec<Generation>,
}

impl ChunkProcessor {
    pub fn new(
        repo: Repo,
        rx: crossbeam_channel::Receiver<Message>,
        aio: aio::AsyncIO,
        encrypter: ArcEncrypter,
        compressor: ArcCompression,
        hasher: ArcHasher,
        generations: Vec<Generation>,
    ) -> Self {
        assert!(!generations.is_empty());
        ChunkProcessor {
            log: repo.log.clone(),
            repo,
            rx,
            aio,
            encrypter,
            compressor,
            hasher,
            generations,
        }
    }

    pub fn run(&self) {
        let mut timer = TimeReporter::new_with_level(
            "chunk-processing",
            self.log.clone(),
            Level::Debug,
        );

        let gen_strings: Vec<_> =
            self.generations.iter().map(|gen| gen.to_string()).collect();

        let last_gen_str = gen_strings.last().unwrap().to_owned();
        loop {
            timer.start("rx");

            if let Ok(input) = self.rx.recv() {
                timer.start("processing");

                let Message {
                    data,
                    response_tx,
                    write_failure_tx,
                    abort_pipeline,
                    data_type,
                } = input;
                let (sg_id, sg) = data;

                let digest = Digest(self.hasher.calculate_digest(&sg));

                if abort_pipeline.load(Ordering::Acquire) {
                    let _ = response_tx.send((sg_id, digest));
                    continue;
                }

                let mut found = false;
                let mut failed = false;
                // lookup all generations in order, starting from current one
                // and at the end try the current gen. again, in case some other
                // thread/ instance just moved it from older generation to the
                // current one
                for gen_str in gen_strings
                    .iter()
                    .rev()
                    .chain([&last_gen_str].iter().cloned())
                {
                    let chunk_path = self.repo.chunk_rel_path_by_digest(
                        digest.as_digest_ref(),
                        gen_str,
                    );
                    match self.aio.read_metadata(chunk_path.clone()).wait() {
                        Ok(_metadata) => {
                            found = true;
                            if gen_str == &last_gen_str {
                                trace!(self.log, "already exists"; "path" => %chunk_path.display());
                            } else {
                                trace!(
                                    self.log,
                                    "already exists in previous generation";
                                    "path" => %chunk_path.display()
                                );
                                let dst_path =
                                    self.repo.chunk_rel_path_by_digest(
                                        digest.as_digest_ref(),
                                        gen_strings.last().unwrap(),
                                    );
                                if let Err(rename_error) = self
                                    .aio
                                    .rename(
                                        chunk_path.clone(),
                                        dst_path.clone(),
                                    )
                                    .wait()
                                {
                                    // Another writer may have moved the chunk
                                    // concurrently; check the destination.
                                    if let Err(destination_error) = self
                                        .aio
                                        .read_metadata(dst_path.clone())
                                        .wait()
                                    {
                                        let error = if destination_error.kind()
                                            == io::ErrorKind::NotFound
                                        {
                                            rename_error
                                        } else {
                                            destination_error
                                        };
                                        report_failure(
                                            &write_failure_tx,
                                            &abort_pipeline,
                                            dst_path,
                                            error,
                                        );
                                        failed = true;
                                    }
                                }
                            }
                            break;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            report_failure(
                                &write_failure_tx,
                                &abort_pipeline,
                                chunk_path,
                                error,
                            );
                            failed = true;
                            break;
                        }
                    }
                }

                if !found && !failed {
                    let chunk_path = self.repo.chunk_rel_path_by_digest(
                        digest.as_digest_ref(),
                        gen_strings.last().unwrap(),
                    );
                    let sg = if data_type.should_compress() {
                        trace!(self.log, "compress"; "path" => %chunk_path.display());
                        timer.start("compress");
                        self.compressor.compress(sg).unwrap()
                    } else {
                        sg
                    };

                    let sg = if data_type.should_encrypt() {
                        trace!(self.log, "encrypt"; "path" => %chunk_path.display());
                        timer.start("encrypt");
                        self.encrypter.encrypt(sg, &digest.0).unwrap()
                    } else {
                        sg
                    };

                    timer.start("tx-writer");
                    let write_path = self.repo.chunk_rel_path_by_digest(
                        digest.as_digest_ref(),
                        &last_gen_str,
                    );
                    if let Err(error) =
                        self.aio.write_idempotent(write_path.clone(), sg).wait()
                    {
                        report_failure(
                            &write_failure_tx,
                            &abort_pipeline,
                            write_path,
                            error,
                        );
                    }
                }
                timer.start("tx-digest");
                response_tx
                    .send((sg_id, digest))
                    .expect("chunk_processor: digests_tx.send")
            } else {
                return;
            }
        }
    }
}

fn report_failure(
    failure_tx: &mpsc::Sender<ChunkWriteFailure>,
    abort_pipeline: &AtomicBool,
    path: PathBuf,
    source: io::Error,
) {
    if is_lease_interruption(&source) {
        abort_pipeline.store(true, Ordering::Release);
    }
    let _ = failure_tx.send(ChunkWriteFailure { path, source });
}

#[cfg(feature = "backend-http")]
fn is_lease_interruption(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|source| {
            source.downcast_ref::<crate::backends::http::HttpBackendError>()
        })
        .is_some_and(|source| {
            matches!(
                source.problem_type(),
                "urn:rdedup:problem:lease-expired"
                    | "urn:rdedup:problem:lease-renewal-closed"
            )
        })
}

#[cfg(not(feature = "backend-http"))]
fn is_lease_interruption(_error: &io::Error) -> bool {
    false
}

#[cfg(all(test, feature = "backend-http"))]
mod tests {
    use super::{report_failure, ChunkWriteFailure};
    use crate::backends::http::HttpBackendError;
    use std::io;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    #[test]
    fn lease_interruption_stops_reading_and_queuing_more_chunks() {
        let (failure_tx, failure_rx) = mpsc::channel::<ChunkWriteFailure>();
        let abort_pipeline = Arc::new(AtomicBool::new(false));
        let error = io::Error::new(
            io::ErrorKind::TimedOut,
            HttpBackendError::LeaseExpired {
                status: 410,
                detail: "lease expired".to_owned(),
            },
        );

        report_failure(
            &failure_tx,
            &abort_pipeline,
            PathBuf::from("chunk-path"),
            error,
        );

        assert!(abort_pipeline.load(Ordering::Acquire));
        assert_eq!(
            failure_rx.recv().unwrap().path,
            PathBuf::from("chunk-path")
        );
    }

    #[test]
    fn ordinary_chunk_failure_does_not_stop_other_chunk_writes() {
        let (failure_tx, failure_rx) = mpsc::channel::<ChunkWriteFailure>();
        let abort_pipeline = Arc::new(AtomicBool::new(false));
        let error = io::Error::other("temporary storage failure");

        report_failure(
            &failure_tx,
            &abort_pipeline,
            PathBuf::from("chunk-path"),
            error,
        );

        assert!(!abort_pipeline.load(Ordering::Acquire));
        assert!(failure_rx.recv().is_ok());
    }
}
