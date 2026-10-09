use crate::iterators::StoredChunks;
use crate::settings;
use crate::util::{ReaderVecIter, WhileOk};
use rand::{self, Rng};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::{Result, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::{self, fs};
use std::{cmp, io};
use url::Url;

mod lib {
    pub use super::super::*;
}

const PASS: &str = "FOO";
const DIGEST_SIZE: usize = 32;

fn rand_tmp_dir() -> PathBuf {
    std::env::temp_dir().join("rdedup-tests").join(
        std::str::from_utf8(
            &rand::rng()
                .sample_iter(&rand::distr::Alphanumeric)
                .take(20)
                .collect::<Vec<_>>()[..],
        )
        .expect("must always be utf8"),
    )
}

fn list_stored_chunks(repo: &lib::Repo) -> Result<HashSet<Vec<u8>>> {
    let operation = (repo.backend_select)()?.begin_shared()?;
    let aio = crate::aio::AsyncIO::new(operation, repo.log.clone())?;
    let mut digests = HashSet::new();
    let data_chunks = StoredChunks::new(
        &aio,
        PathBuf::from("."),
        DIGEST_SIZE,
        repo.log.clone(),
    )?;
    for digest in data_chunks {
        let digest = digest?;
        digests.insert(digest);
    }
    Ok(digests)
}

fn test_repo(pass: &str) -> lib::Repo {
    let mut settings = settings::Repo::new();
    // Make it fasts to use
    settings.set_pwhash(settings::PWHash::Weak);
    let url = Url::from_file_path(rand_tmp_dir()).unwrap();
    lib::Repo::init_from_url(Arc::new(url), &|| Ok(pass.into()), settings, None)
        .unwrap()
}

fn test_repo_dir(pass: &str) -> (lib::Repo, PathBuf) {
    let mut settings = settings::Repo::new();
    // Make it fasts to use
    settings.set_pwhash(settings::PWHash::Weak);
    let dir = rand_tmp_dir();
    let url = Url::from_file_path(&dir).unwrap();
    (
        lib::Repo::init_from_url(
            Arc::new(url),
            &|| Ok(pass.into()),
            settings,
            None,
        )
        .unwrap(),
        dir,
    )
}

/// Generate data that repease some chunks
struct ExampleDataGen {
    a: Vec<u8>,
    b: Vec<u8>,
    c: Vec<u8>,
    count: usize,
    sha: Sha256,
}

impl ExampleDataGen {
    fn new(kb: usize) -> Self {
        ExampleDataGen {
            a: rand_data(1024 * 2),
            b: rand_data(1024 * 2),
            c: rand_data(1024 * 2),
            count: kb,
            sha: Sha256::default(),
        }
    }

    fn finish(self) -> Vec<u8> {
        let mut vec_result = vec![0u8; DIGEST_SIZE];
        vec_result.copy_from_slice(&self.sha.finalize());
        vec_result
    }
}

fn copy_as_much_as_possible(dst: &mut [u8], src: &[u8]) -> usize {
    let len = cmp::min(dst.len(), src.len());
    dst[..len].clone_from_slice(&src[..len]);
    len
}

impl io::Read for ExampleDataGen {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if self.count == 0 {
            return Ok(0);
        }
        self.count -= 1;

        let len = match rand::rng().random_range(0..3) {
            0 => copy_as_much_as_possible(buf, &self.a),
            1 => copy_as_much_as_possible(buf, &self.b),
            2 => copy_as_much_as_possible(buf, &self.c),
            _ => panic!(),
        };

        self.sha.update(&buf[..len]);

        Ok(len)
    }
}

fn rand_data(len: usize) -> Vec<u8> {
    rand::rng()
        .sample_iter(&rand::distr::StandardUniform)
        .take(len)
        .collect::<Vec<u8>>()
}

fn wipe(repo: &lib::Repo) {
    let names = repo.list_names().unwrap();

    for name in &names {
        println!("Wiping name: {}", name);
        repo.rm(name).unwrap();
    }

    println!("Final GC");
    repo.gc(0).unwrap();

    assert_eq!(list_stored_chunks(repo).unwrap().len(), 0);
    assert_eq!(repo.list_reachable_chunks().unwrap().len(), 0);
}

#[test]
fn zero_size() {
    let repo = test_repo(PASS);
    {
        let zero = Vec::new();
        let enc_handle = repo.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
        repo.write("zero", io::Cursor::new(zero), &enc_handle)
            .unwrap();
    }

    let mut read_zero = Vec::new();
    {
        let dec_handle = repo.unlock_decrypt(&|| Ok(PASS.into())).unwrap();
        repo.read("zero", &mut read_zero, &dec_handle).unwrap();
    }

    assert_eq!(read_zero.len(), 0);

    wipe(&repo);
}

#[test]
fn byte_size() {
    let repo = test_repo(PASS);

    let enc_handle = repo.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
    let dec_handle = repo.unlock_decrypt(&|| Ok(PASS.into())).unwrap();

    let tests = [0u8, 1, 13, 255];
    for &b in &tests {
        let data = vec![b];
        let name = hex::encode(&data);
        repo.write(&name, io::Cursor::new(&data), &enc_handle)
            .unwrap();
    }
    for &b in &tests {
        let mut data = Vec::new();
        let name = hex::encode(vec![b]);
        repo.read(name.as_str(), &mut data, &dec_handle).unwrap();
        assert_eq!(data, vec![b]);
    }

    wipe(&repo);
}

#[test]
#[ignore] // too slow to run by default
fn random_sanity() {
    let mut names = vec![];

    let repo = test_repo(PASS);
    let enc_handle = repo.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
    let dec_handle = repo.unlock_decrypt(&|| Ok(PASS.into())).unwrap();

    for i in 0..10 {
        let mut data =
            ExampleDataGen::new(rand::rng().random_range(0..10 * 1024));
        let name = format!("{:x}", i);
        repo.write(&name, &mut data, &enc_handle).unwrap();
        names.push((name, data.finish()));
    }

    repo.gc(0).unwrap();

    for (name, digest) in &names {
        let mut data = vec![];
        repo.read(name.as_str(), &mut data, &dec_handle).unwrap();

        let mut sha = Sha256::default();
        sha.update(&data);
        let mut read_digest = vec![0u8; DIGEST_SIZE];
        read_digest.copy_from_slice(&sha.finalize());
        assert_eq!(digest, &read_digest);
    }

    for (name, digest) in names.drain(..) {
        repo.gc(0).unwrap();

        {
            let mut data = vec![];
            repo.read(name.as_str(), &mut data, &dec_handle).unwrap();

            let mut sha = Sha256::default();
            sha.update(&data);
            let mut read_digest = vec![0u8; DIGEST_SIZE];
            read_digest.copy_from_slice(&sha.finalize());
            assert_eq!(&digest, &read_digest);
        }

        let reachable = repo.list_reachable_chunks().unwrap();
        let stored = list_stored_chunks(&repo).unwrap();

        assert_eq!(reachable.len(), stored.len());

        for digest in reachable.iter() {
            assert!(stored.contains(digest));
        }
        for digest in stored.iter() {
            assert!(reachable.contains(digest));
        }

        repo.rm(&name).unwrap();
        let reachable_after_rm = repo.list_reachable_chunks().unwrap();
        let stored_after_rm = list_stored_chunks(&repo).unwrap();

        assert_eq!(stored_after_rm.len(), stored.len());
        assert!(reachable_after_rm.len() < reachable.len());
    }

    wipe(&repo);
}

#[test]
fn change_passphrase() {
    let mut prev_passphrase = "foo";
    let dir_path = &rand_tmp_dir();
    let data_before = rand_data(1024);

    {
        let mut settings = settings::Repo::new();
        settings.set_pwhash(settings::PWHash::Weak);
        let repo = lib::Repo::init_from_url(
            Arc::new(Url::from_file_path(dir_path).unwrap()),
            &|| Ok(prev_passphrase.into()),
            settings,
            None,
        )
        .unwrap();

        let enc_handle =
            repo.unlock_encrypt(&|| Ok(prev_passphrase.into())).unwrap();

        repo.write("data", io::Cursor::new(&data_before), &enc_handle)
            .unwrap();
    }

    for &p in &["a", "", "foo", "bar"] {
        let mut repo = lib::Repo::open_from_url(
            Arc::new(Url::from_file_path(dir_path).unwrap()),
            None,
        )
        .unwrap();
        repo.change_passphrase(
            &|| Ok(prev_passphrase.into()),
            &|| Ok(p.into()),
        )
        .unwrap();
        prev_passphrase = p;
    }

    {
        let repo = lib::Repo::open_from_url(
            Arc::new(Url::from_directory_path(dir_path).unwrap()),
            None,
        )
        .unwrap();
        let dec_handle =
            repo.unlock_decrypt(&|| Ok(prev_passphrase.into())).unwrap();
        let mut data_after = vec![];
        repo.read("data", &mut data_after, &dec_handle).unwrap();

        assert_eq!(data_before, data_after);
    }

    let repo = lib::Repo::open_from_url(
        Arc::new(Url::from_file_path(dir_path).unwrap()),
        None,
    )
    .unwrap();
    wipe(&repo);
}

#[test]
fn verify_name() {
    let (repo, dir) = test_repo_dir(PASS);

    let dec_handle = repo.unlock_decrypt(&|| Ok(PASS.into())).unwrap();
    let enc_handle = repo.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
    let data = rand_data(1024);
    {
        repo.write("data", io::Cursor::new(&data), &enc_handle)
            .unwrap();
    }

    let mut result = repo.verify("data", &dec_handle).unwrap();
    assert_eq!(result.errors.len(), 0);

    // Corrupt first chunk we find
    let generations = {
        let operation =
            (repo.backend_select)().unwrap().begin_shared().unwrap();
        let aio =
            crate::aio::AsyncIO::new(operation, repo.log.clone()).unwrap();
        repo.read_generations(&aio).unwrap()
    };

    let chunk_path = dir.join(generations[0].to_string()).join("chunk");
    for l1 in fs::read_dir(&chunk_path).unwrap() {
        let l1 = l1.unwrap();
        if l1.path().is_dir() {
            for l2 in fs::read_dir(l1.path()).unwrap() {
                let l2 = l2.unwrap();
                if l2.path().is_dir() {
                    for l3 in fs::read_dir(l2.path()).unwrap() {
                        let l3 = l3.unwrap();
                        let mut chunk = OpenOptions::new()
                            .append(true)
                            .open(l3.path())
                            .unwrap();
                        chunk.write_all(&[1]).unwrap();
                    }
                }
            }
        }
    }

    result = repo.verify("data", &dec_handle).unwrap();
    assert_eq!(result.errors.len(), 1);

    wipe(&repo);
}

#[test]
fn test_stored_chunks_iter() {
    let repo = test_repo(PASS);
    let data = rand_data(1024 * 1024);

    let enc_handle = repo.unlock_encrypt(&|| Ok(PASS.into())).unwrap();

    repo.write("data", io::Cursor::new(&data), &enc_handle)
        .unwrap();
    let chunks_from_indexes = repo.list_reachable_chunks().unwrap();

    let mut chunks_from_iter = list_stored_chunks(&repo).unwrap();
    assert_eq!(chunks_from_indexes.len(), chunks_from_iter.len());
    assert_eq!(chunks_from_indexes.difference(&chunks_from_iter).count(), 0);

    // Add a second name to the repo and compare chunks
    let data2 = rand_data(1024 * 1024);
    repo.write("data2", io::Cursor::new(&data2), &enc_handle)
        .unwrap();
    let chunks_from_indexes2 = repo.list_reachable_chunks().unwrap();
    chunks_from_iter = list_stored_chunks(&repo).unwrap();
    assert_eq!(chunks_from_indexes.difference(&chunks_from_iter).count(), 0);

    // Remove the second name and make sure the difference
    repo.rm("data2").unwrap();
    let chunks_from_indexes3 = repo.list_reachable_chunks().unwrap();
    assert_eq!(
        chunks_from_indexes3
            .difference(&chunks_from_indexes)
            .count(),
        0
    );
    // Chunks from iterator should equal the list from both names before the
    // removal
    chunks_from_iter = list_stored_chunks(&repo).unwrap();
    assert_eq!(
        chunks_from_indexes2.difference(&chunks_from_iter).count(),
        0
    );

    repo.gc(0).unwrap();
    // Chunks from iterator should equal the first reachable list
    chunks_from_iter = list_stored_chunks(&repo).unwrap();
    assert_eq!(chunks_from_indexes.difference(&chunks_from_iter).count(), 0);
}

#[test]
fn test_custom_chunking_size() {
    for &bits in &[9, 10, 17, 20, 30, 31] {
        let dir_path = rand_tmp_dir();
        {
            let mut settings = settings::Repo::new();

            let result = settings.use_bup_chunking(Some(bits));

            if !(10..=30).contains(&bits) {
                if result.is_err() {
                    continue;
                } else {
                    panic!("expected an error for value {:}, but got Ok", bits);
                }
            } else if result.is_err() {
                panic!("expected Ok, but got {:}", result.err().unwrap());
            }
            settings.set_pwhash(settings::PWHash::Weak);
            lib::Repo::init_from_url(
                Arc::new(Url::from_file_path(dir_path.clone()).unwrap()),
                &|| Ok(PASS.into()),
                settings.clone(),
                None,
            )
            .unwrap();

            let repo = lib::Repo::open_from_url(
                Arc::new(Url::from_directory_path(dir_path).unwrap()),
                None,
            )
            .unwrap();
            assert_eq!(settings.chunking.0, repo.config.chunking);
            wipe(&repo);
        }
    }
}

#[test]
fn test_custom_nesting() {
    for &level in &[0, 1, 4, 31, 64] {
        let dir_path = rand_tmp_dir();
        {
            let mut settings = settings::Repo::new();
            let result = settings.set_nesting(level);

            if level > 31 {
                if result.is_err() {
                    continue;
                } else {
                    panic!(
                        "expected an error for value {:}, but got Ok",
                        level
                    );
                }
            } else if result.is_err() {
                panic!("expected Ok, but got {:}", result.err().unwrap());
            }
            settings.set_pwhash(settings::PWHash::Weak);
            lib::Repo::init_from_url(
                Arc::new(Url::from_file_path(dir_path.clone()).unwrap()),
                &|| Ok(PASS.into()),
                settings.clone(),
                None,
            )
            .unwrap();

            let repo = lib::Repo::open_from_url(
                Arc::new(Url::from_file_path(dir_path).unwrap()),
                None,
            )
            .unwrap();
            assert_eq!(lib::config::Nesting(level), repo.config.nesting);

            // Test Store, Load, RM, and GC
            let data = rand_data(1024 * 1024);
            let enc_handle = repo.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
            let dec_handle = repo.unlock_decrypt(&|| Ok(PASS.into())).unwrap();

            repo.write("data", io::Cursor::new(&data), &enc_handle)
                .unwrap();

            let mut load_data = vec![];
            repo.read("data", &mut load_data, &dec_handle).unwrap();

            assert_eq!(load_data, data);

            repo.rm("data").unwrap();

            repo.gc(0).unwrap();

            wipe(&repo);
        }
    }
}

#[test]
fn test_readerveciter() {
    let input = vec![0, 1, 2, 3, 4];
    let r2vi = ReaderVecIter::new(input.as_slice(), 2);
    let mut while_ok = WhileOk::new(r2vi);

    let v: Vec<Vec<_>> = (&mut while_ok).collect();

    assert_eq!(v, [vec![0, 1], vec![2, 3], vec![4]]);
    assert!(while_ok.finish().is_none());

    let r2vi = ReaderVecIter::new(input.as_slice(), 2);
    let r2vi_e = r2vi.map(|x| match x {
        Ok(ref v) if *v == vec![2, 3] => Err(io::Error::other("error")),
        x => x,
    });
    let mut while_ok = WhileOk::new(r2vi_e);

    let v: Vec<Vec<_>> = (&mut while_ok).collect();

    assert_eq!(v, [vec![0, 1]]);
    assert!(while_ok.finish().is_some());
}

struct FailingLockBackend;

impl lib::backends::Backend for FailingLockBackend {
    fn begin_exclusive(
        &self,
    ) -> io::Result<lib::backends::BackendOperation<lib::backends::Exclusive>>
    {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock denied",
        ))
    }

    fn begin_shared(
        &self,
    ) -> io::Result<lib::backends::BackendOperation<lib::backends::Shared>>
    {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock denied",
        ))
    }
}

fn repo_with_failing_locks() -> lib::Repo {
    let mut repository = test_repo(PASS);
    repository.backend_select = Arc::new(|| Ok(Box::new(FailingLockBackend)));
    repository
}

#[test]
fn shared_repository_operations_fail_when_the_lock_cannot_be_acquired() {
    let repository = repo_with_failing_locks();

    let error = match repository.list_names() {
        Err(error) => error,
        Ok(names) => panic!("listing unexpectedly succeeded: {names:?}"),
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn exclusive_repository_operations_fail_when_the_lock_cannot_be_acquired() {
    let repository = repo_with_failing_locks();

    let error = match repository.rm("missing") {
        Err(error) => error,
        Ok(()) => panic!("remove unexpectedly succeeded"),
    };
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[test]
fn retrying_a_name_write_with_the_same_contents_succeeds() {
    let repository = test_repo(PASS);
    let encryption = repository.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
    let data = b"retry-safe archive contents";

    repository
        .write("archive", io::Cursor::new(data), &encryption)
        .unwrap();
    repository
        .write("archive", io::Cursor::new(data), &encryption)
        .expect("retrying the same name and contents should succeed");

    let mut stored_data = Vec::new();
    let decryption = repository.unlock_decrypt(&|| Ok(PASS.into())).unwrap();
    repository
        .read("archive", &mut stored_data, &decryption)
        .unwrap();
    assert_eq!(stored_data, data);

    wipe(&repository);
}

#[test]
fn conflicting_name_write_preserves_the_original_contents() {
    let repository = test_repo(PASS);
    let encryption = repository.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
    let original_data = b"original archive contents";

    repository
        .write("archive", io::Cursor::new(original_data), &encryption)
        .unwrap();
    let error = repository
        .write(
            "archive",
            io::Cursor::new(b"different archive contents"),
            &encryption,
        )
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

    let mut stored_data = Vec::new();
    let decryption = repository.unlock_decrypt(&|| Ok(PASS.into())).unwrap();
    repository
        .read("archive", &mut stored_data, &decryption)
        .unwrap();
    assert_eq!(stored_data, original_data);

    wipe(&repository);
}

#[test]
fn reading_a_legacy_name_does_not_rewrite_it() {
    let (repository, directory) = test_repo_dir(PASS);
    let encryption = repository.unlock_encrypt(&|| Ok(PASS.into())).unwrap();
    let decryption = repository.unlock_decrypt(&|| Ok(PASS.into())).unwrap();
    repository
        .write("legacy", io::Cursor::new(b"archive bytes"), &encryption)
        .unwrap();
    let operation = (repository.backend_select)()
        .unwrap()
        .begin_shared()
        .unwrap();
    let aio =
        crate::aio::AsyncIO::new(operation, repository.log.clone()).unwrap();
    let generations = repository.read_generations(&aio).unwrap();
    let path = directory.join(crate::name::Name::path(
        "legacy",
        *generations.last().unwrap(),
    ));
    drop(aio);
    let mut name: serde_yaml::Value =
        serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap();
    name.as_mapping_mut()
        .unwrap()
        .remove(serde_yaml::Value::String("created".into()));
    let legacy = serde_yaml::to_string(&name).unwrap();
    fs::write(&path, &legacy).unwrap();
    let mut contents = Vec::new();
    repository
        .read("legacy", &mut contents, &decryption)
        .unwrap();
    assert_eq!(contents, b"archive bytes");
    assert_eq!(fs::read_to_string(&path).unwrap(), legacy);
    wipe(&repository);
}
