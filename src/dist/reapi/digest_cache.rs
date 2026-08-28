// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Remembering the content hash of a file we have already hashed.
//!
//! This is the piece of daemon-resident state that makes staging inputs
//! individually cheap. A C++ build asks about the same headers over and over:
//! every translation unit in LLVM pulls in several hundred, and almost all of
//! them are shared with hundreds of other translation units. Hashing them
//! once per compile is the dominant cost of building an input root.
//!
//! So we do what the daemon is *for*: keep the answer. Ask the filesystem
//! whether the file still looks the way it did -- same inode, same size, same
//! timestamps -- and if so, hand back the digest without reading a byte.
//! Measured on one LLVM translation unit (500 inputs, 9.2 MB), hashing costs
//! ~10ms and the `stat` calls that replace it cost ~1.2ms.
//!
//! Note that this is only possible because the input list arrives as *paths*.
//! When inputs arrive as a tar, every byte has already been copied through
//! the archive before we ever see it, and the work this cache would save has
//! by then already been done twice.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use crate::errors::*;

use super::{merkle, proto::build::bazel::remote::execution::v2 as reapi};

/// What we compare against to decide whether a remembered digest still
/// applies.
///
/// The tuple is the conventional one -- ccache's direct mode and Bazel's
/// local file digest cache both key on the same kind of stat data. `ctime` is
/// in here alongside `mtime` deliberately: a file whose contents were swapped
/// for different contents of the same length, with the mtime restored, would
/// otherwise look unchanged. Restoring `ctime` requires privileges, so
/// including it closes the cheap version of that hole. It cannot close all of
/// it -- a filesystem with coarse timestamps and an adversarial writer can
/// still fool this, which is the same exposure every stat-based build cache
/// carries.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct FileKey {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

#[derive(Clone, Debug)]
pub struct CachedDigest {
    pub digest: reapi::Digest,
    pub is_executable: bool,
}

/// Bound on how many files we remember.
///
/// A large C++ build touches on the order of tens of thousands of distinct
/// headers, so this holds a whole build's working set while staying a few MB.
/// The eviction policy is deliberately crude -- see [`DigestCache::insert`].
const MAX_ENTRIES: usize = 200_000;

#[derive(Default)]
pub struct DigestCache {
    map: Mutex<HashMap<FileKey, CachedDigest>>,
}

impl DigestCache {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The digest of `path`, hashing it only if we have not seen this exact
    /// file before.
    ///
    /// Runs blocking filesystem work, so call it from a blocking context.
    pub fn digest_of(&self, path: &Path) -> Result<CachedDigest> {
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("Failed to stat input {path:?}"))?;

        // A symlink's own metadata is not the content we would hash, and a
        // directory has no content at all. Callers only pass regular files;
        // refuse anything else rather than silently hashing the wrong thing.
        if !meta.is_file() {
            bail!("Input {path:?} is not a regular file");
        }

        let key = FileKey::of(&meta);

        if let Some(hit) = self.map.lock().unwrap().get(&key) {
            return Ok(hit.clone());
        }

        let data = std::fs::read(path).with_context(|| format!("Failed to read input {path:?}"))?;

        // Re-stat after reading. If the file changed underneath us the digest
        // we just computed does not belong to `key`, and caching it would
        // poison every later lookup. Dropping it is enough: the digest itself
        // is still correct for the bytes we read, and those are the bytes we
        // would upload.
        let changed = std::fs::symlink_metadata(path)
            .map(|after| FileKey::of(&after) != key)
            .unwrap_or(true);

        let entry = CachedDigest {
            digest: merkle::digest_bytes(&data),
            is_executable: is_executable(&meta),
        };

        if !changed {
            self.insert(key, entry.clone());
        }

        Ok(entry)
    }

    /// Read a file that we already know the digest of, for upload.
    ///
    /// Separate from [`Self::digest_of`] because the two happen at different
    /// times: everything gets digested, and then only the small subset the
    /// server reports missing gets read again.
    pub fn read_for_upload(&self, path: &Path, digest: &reapi::Digest) -> Result<Vec<u8>> {
        let data = std::fs::read(path).with_context(|| format!("Failed to read input {path:?}"))?;
        let actual = merkle::digest_bytes(&data);
        if &actual != digest {
            // The file changed between building the input root and uploading
            // it. Uploading these bytes under the old digest would corrupt
            // the CAS for every other client, so fail the job instead and let
            // the retry rebuild the tree.
            bail!(
                "Input {path:?} changed while the job was being prepared \
                 (expected {}, found {})",
                digest.hash,
                actual.hash
            );
        }
        Ok(data)
    }

    fn insert(&self, key: FileKey, entry: CachedDigest) {
        let mut map = self.map.lock().unwrap();
        // Crude, but the access pattern makes it fine: a build's header set
        // is reached quickly and then stays hot, so the cache either fits or
        // the build is large enough that any policy would be thrashing. This
        // avoids paying for LRU bookkeeping on the hot path.
        if map.len() >= MAX_ENTRIES {
            map.clear();
        }
        map.insert(key, entry);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.lock().unwrap().len()
    }
}

#[cfg(unix)]
impl FileKey {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_ns: meta.ctime_nsec(),
        }
    }
}

#[cfg(not(unix))]
impl FileKey {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::time::UNIX_EPOCH;
        let secs_nanos = |t: std::io::Result<std::time::SystemTime>| {
            t.ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| (d.as_secs() as i64, d.subsec_nanos() as i64))
                .unwrap_or((0, 0))
        };
        let (mtime, mtime_ns) = secs_nanos(meta.modified());
        let (ctime, ctime_ns) = secs_nanos(meta.created());
        Self {
            // No stable device/inode pair off Unix; the path-independent
            // parts still make this safe, just less selective.
            dev: 0,
            ino: 0,
            size: meta.len(),
            mtime,
            mtime_ns,
            ctime,
            ctime_ns,
        }
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, data: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(data).unwrap();
        path
    }

    #[test]
    fn repeated_lookups_reuse_one_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.h", b"#pragma once\n");
        let cache = DigestCache::default();

        let first = cache.digest_of(&path).unwrap();
        let second = cache.digest_of(&path).unwrap();

        assert_eq!(first.digest, second.digest);
        assert_eq!(first.digest, merkle::digest_bytes(b"#pragma once\n"));
        // One file hashed, one entry -- the second lookup was a hit.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn rewriting_a_file_invalidates_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.h", b"one");
        let cache = DigestCache::default();
        let before = cache.digest_of(&path).unwrap();

        // Same length, different content: only the timestamps distinguish
        // these, which is exactly the case the key exists to catch.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write(dir.path(), "a.h", b"two");

        let after = cache.digest_of(&path).unwrap();
        assert_ne!(before.digest, after.digest);
        assert_eq!(after.digest, merkle::digest_bytes(b"two"));
    }

    #[test]
    fn distinct_files_get_distinct_entries() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a.h", b"aaa");
        let b = write(dir.path(), "b.h", b"bbb");
        let cache = DigestCache::default();

        assert_ne!(
            cache.digest_of(&a).unwrap().digest,
            cache.digest_of(&b).unwrap().digest
        );
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn upload_read_rejects_a_file_that_changed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "a.h", b"one");
        let cache = DigestCache::default();
        let digest = cache.digest_of(&path).unwrap().digest;

        assert_eq!(cache.read_for_upload(&path, &digest).unwrap(), b"one");

        write(dir.path(), "a.h", b"three");
        let err = cache.read_for_upload(&path, &digest).unwrap_err();
        assert!(
            err.to_string().contains("changed while the job"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = DigestCache::default();
        assert!(cache.digest_of(dir.path()).is_err());
    }
}
