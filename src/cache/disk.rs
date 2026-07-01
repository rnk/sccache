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

use super::lazy_disk_cache::LazyDiskCache;
use crate::{
    cache::{Cache, CacheMode, GetPathResult, Storage},
    config::DiskCacheConfig,
    lru_disk_cache::Error as LruError,
};
use async_trait::async_trait;
use bytes::{Buf, Bytes};
use futures::lock::Mutex;
use memmap2::Mmap;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::errors::*;

/// A cache that stores entries at local disk paths.
pub struct DiskCache {
    /// `LruDiskCache` does all the real work here.
    lru: Arc<Mutex<LazyDiskCache>>,
    rw_mode: CacheMode,
    basedirs: Vec<Vec<u8>>,
}

impl DiskCache {
    /// Create a new `DiskCache` rooted at `root`, with `max_size` as the maximum cache size on-disk, in bytes.
    pub fn new<T: AsRef<OsStr>>(
        root: T,
        max_size: u64,
        rw_mode: CacheMode,
        basedirs: Vec<Vec<u8>>,
    ) -> Self {
        DiskCache {
            lru: Arc::new(Mutex::new(LazyDiskCache::Uninit {
                root: root.as_ref().to_os_string(),
                max_size,
            })),
            rw_mode,
            basedirs,
        }
    }

    async fn file(&self, key: &str) -> Result<fs_err::File> {
        match self.lru.lock().await.get_or_init()?.get_file(key) {
            Ok(reader) => Ok(reader),
            Err(LruError::Io(err)) => Err(err.into()),
            Err(err) => Err(err.into()),
        }
    }
}

impl DiskCache {
    pub async fn entry(&self, key: &str) -> Result<(PathBuf, u64)> {
        self.lru.lock().await.get_or_init().and_then(|lru| {
            let path = lru.key_to_abs_path(key);
            lru.get_size(key)
                .with_context(|| format!("[DiskCache::entry({key})]: {path:?}"))
                .map(|size| (path, size))
        })
    }

    pub async fn insert_with<F, Fut>(
        &self,
        key: &str,
        size: u64,
        with_fn: F,
    ) -> Result<(PathBuf, u64)>
    where
        F: FnOnce(&Path) -> Fut,
        Fut: std::future::Future<Output = std::io::Result<u64>>,
    {
        // Separate lru locks so concurrent insertions aren't serialized

        let tmp = self
            .lru
            .lock()
            .await
            .get_or_init()?
            .prepare_dir(key, size)
            .await
            .with_context(|| format!("[DiskCache::insert_with({key}, {size})]"))?;

        let tmp_path = tmp.as_path();

        // Do the actual I/O now that the LRU lock dropped
        let res = with_fn(tmp_path).await.map_err(anyhow::Error::new);

        // Clean up and commit the dir
        self.lru
            .lock()
            .await
            .get_or_init()?
            .commit_dir(res.as_ref().map(|_| &tmp).map_err(|_| &tmp))
            .await
            .or_else(|err| {
                if let LruError::FileNotInCache = err {
                    // Usual case: return the original (possibly I/O) error.
                    // Map successful results to this function's return signature,
                    // even though  `FileNotInCache` means `res` will always be Err
                    res.map(|_| (PathBuf::new(), size))
                } else {
                    // Unusual case: encountered an IO error committing the directory
                    Err(err.into())
                }
            })
            .with_context(|| format!("[DiskCache::insert_with({key}, {size})]: {tmp_path:?}"))
    }
}

#[async_trait]
impl Storage for DiskCache {
    async fn get(&self, key: &str) -> Result<Cache<opendal::Buffer>> {
        match self.file(key).await {
            Ok(file) => Ok(Cache::Hit(
                Bytes::from_owner(unsafe { Mmap::map(&file) }?).into(),
            )),
            Err(err) => match err.downcast_ref::<LruError>() {
                Some(LruError::FileNotInCache) => Ok(Cache::Miss),
                _ => Err(err),
            },
        }
    }

    async fn del(&self, key: &str) -> Result<()> {
        if self.rw_mode == CacheMode::ReadOnly {
            return Err(anyhow!("Cannot write to read-only storage"));
        }

        match self.lru.lock().await.get_or_init() {
            Err(err) => Err(err),
            Ok(lru) => lru
                .remove(key)
                .await
                .with_context(|| format!("[DiskCache::del({key})]")),
        }
    }

    async fn has(&self, key: &str) -> bool {
        self.size(key).await.is_ok()
    }

    async fn put(&self, key: &str, data: opendal::Buffer) -> Result<Duration> {
        trace!("DiskCache::put({key})");
        if self.rw_mode == CacheMode::ReadOnly {
            return Err(anyhow!("Cannot write to read-only storage"));
        }

        let start = Instant::now();

        let mut f = self
            .lru
            .lock()
            .await
            .get_or_init()?
            .prepare_add(key, 0)
            .await
            .with_context(|| format!("[DiskCache::put({key})]"))?;

        // Copy source into the tempfile
        let source = futures::io::AllowStdIo::new(data.reader());
        futures::io::copy_buf(
            source,
            &mut futures::io::AllowStdIo::new(std::io::BufWriter::new(f.as_file_mut())),
        )
        .await
        .with_context(|| format!("[DiskCache::put({key})]"))?;

        self.lru
            .lock()
            .await
            .get_or_init()?
            .commit(f)
            .await
            .with_context(|| format!("[DiskCache::put({key})]"))?;

        Ok(start.elapsed())
    }

    async fn get_path(&self, key: &str) -> GetPathResult {
        self.lru
            .lock()
            .await
            .get_or_init()
            .ok()
            .and_then(|lru| lru.get_abs_path(key))
            .map_or_else(|| GetPathResult::Miss, GetPathResult::Found)
    }

    async fn size(&self, key: &str) -> Result<u64> {
        self.lru.lock().await.get_or_init().and_then(|lru| {
            lru.get_size(key)
                .with_context(|| format!("[DiskCache::size({key})]"))
        })
    }

    async fn check(&self) -> Result<CacheMode> {
        Ok(self.rw_mode)
    }

    async fn location(&self) -> String {
        format!("Local disk: {:?}", self.lru.lock().await.path())
    }

    fn cache_type_name(&self) -> &'static str {
        "disk"
    }

    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(self.lru.lock().await.get().map(|l| l.size()))
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        &self.basedirs
    }

    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(Some(self.lru.lock().await.capacity()))
    }
}

impl From<&DiskCacheConfig> for Arc<dyn Storage> {
    fn from(config: &DiskCacheConfig) -> Self {
        Arc::new(DiskCache::new(
            &config.dir,
            config.size,
            config.rw_mode.into(),
            vec![],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_cache_type_name() {
        let tempdir = tempfile::tempdir().unwrap();
        let disk = DiskCache::new(tempdir.path(), 1024 * 1024, CacheMode::ReadWrite, vec![]);

        assert_eq!(disk.cache_type_name(), "disk");
    }
}
