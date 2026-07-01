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

use crate::{
    cache::{
        disk::DiskCache,
        multilevel::{MultiLevelStats, MultiLevelStorage},
        readonly::ReadOnlyStorage,
        utils::normalize_key,
    },
    config::{CacheConfigs, CacheType, DiskCacheConfig, PreprocessorCacheModeConfig},
    errors::*,
};
use async_trait::async_trait;
use futures::{FutureExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use super::cache_io::*;

#[cfg(feature = "watcher")]
use crate::cache::watch::WatchStorage;
#[cfg(feature = "azure")]
use crate::{cache::azure::AzureBlobCache, config::AzureCacheConfig};
#[cfg(feature = "cos")]
use crate::{cache::cos::COSCache, config::COSCacheConfig};
#[cfg(feature = "gcs")]
use crate::{cache::gcs::GCSCache, config::GCSCacheConfig};
#[cfg(feature = "gha")]
use crate::{cache::gha::GHACache, config::GHACacheConfig};
#[cfg(feature = "memcached")]
use crate::{cache::memcached::MemcachedCache, config::MemcachedCacheConfig};
#[cfg(feature = "oss")]
use crate::{cache::oss::OSSCache, config::OSSCacheConfig};
#[cfg(feature = "redis")]
use crate::{
    cache::redis::RedisCache,
    config::{DEFAULT_REDIS_DB, RedisCacheConfig},
};
#[cfg(feature = "s3")]
use crate::{cache::s3::S3Cache, config::S3CacheConfig};
#[cfg(feature = "webdav")]
use crate::{cache::webdav::WebdavCache, config::WebdavCacheConfig};

/// Result of [`Storage::get_path`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GetPathResult {
    /// Cache hit: the entry lives at this filesystem path.
    Found(PathBuf),
    /// Cache miss: the key is not in the cache.
    Miss,
    /// This backend does not support direct file access; use `get`/`get_raw` instead.
    Unsupported,
}

/// An interface to cache storage.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Get a cache entry by `key`.
    ///
    /// If an error occurs, this method should return a `Result::Err`.
    /// If nothing fails but the entry is not found in the cache,
    /// it should return a `Cache::Miss`.
    /// If the entry is successfully found in the cache, it should
    /// return a `Cache::Hit`.
    async fn get(&self, key: &str) -> Result<Cache<opendal::Buffer>>;

    /// Delete the cache entry for `key`.
    async fn del(&self, key: &str) -> Result<()>;

    /// Check if the cache has an entry for `key`.
    ///
    /// If the entry is successfully found in the cache, return true.
    /// If an error occurs, or the entry is not found in the cache, return false.
    async fn has(&self, key: &str) -> bool;

    /// Put `entry` in the cache under `key`.
    ///
    /// Returns a `Future` that will provide the result or error when the put is
    /// finished.
    async fn put(&self, key: &str, entry: opendal::Buffer) -> Result<Duration>;

    async fn size(&self, key: &str) -> Result<u64>;

    /// Get raw serialized cache entry bytes by `key` (for multi-level backfill).
    /// Returns `None` if the entry is not found, or if the implementation doesn't support raw access.
    /// This is used by multi-level caches to backfill faster levels.
    async fn get_raw(&self, _key: &str) -> Result<Option<opendal::Buffer>>;

    /// Put raw serialized cache entry bytes under `key` (for multi-level backfill).
    /// Returns an error if the implementation doesn't support raw access.
    /// This is used by multi-level caches to backfill faster levels.
    async fn put_raw(&self, _key: &str, _data: opendal::Buffer) -> Result<Duration>;

    /// Check the cache capability.
    ///
    /// - `Ok(CacheMode::ReadOnly)` means cache can only be used to `get`
    ///   cache.
    /// - `Ok(CacheMode::ReadWrite)` means cache can do both `get` and `put`.
    /// - `Err(err)` means cache is not setup correctly or not match with
    ///   users input (for example, user try to use `ReadWrite` but cache
    ///   is `ReadOnly`).
    ///
    /// We will provide a default implementation which returns
    /// `Ok(CacheMode::ReadWrite)` for service that doesn't
    /// support check yet.
    async fn check(&self) -> Result<CacheMode> {
        Ok(CacheMode::ReadWrite)
    }

    /// Get the storage location.
    async fn location(&self) -> String;

    /// Get the cache backend type name (e.g., "disk", "redis", "s3").
    /// Used for statistics and display purposes.
    fn cache_type_name(&self) -> &'static str;

    /// Get the current storage usage, if applicable.
    async fn current_size(&self) -> Result<Option<u64>>;

    /// Get the maximum storage size, if applicable.
    async fn max_size(&self) -> Result<Option<u64>>;

    /// Get multi-level cache statistics, if this is a multi-level storage.
    fn multilevel_stats(&self) -> Option<MultiLevelStats> {
        None
    }

    /// Return the config for preprocessor cache mode if applicable
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        PreprocessorCacheModeConfig::default()
    }

    /// Return the base directories for path normalization if configured
    fn basedirs(&self) -> &[Vec<u8>];

    /// Return the filesystem path of the cached entry for `key`.
    /// Default impl returns [`GetPathResult::Unsupported`].
    async fn get_path(&self, _key: &str) -> GetPathResult {
        GetPathResult::Unsupported
    }
}

#[cfg(any(
    feature = "azure",
    feature = "gcs",
    feature = "gha",
    feature = "memcached",
    feature = "redis",
    feature = "s3",
    feature = "webdav",
    feature = "oss",
    feature = "cos"
))]
mod operator {
    use super::*;
    use crate::util::retry_with_jitter;
    use tokio_retry2::RetryError;

    fn to_retry_err<E: Into<opendal::Error>>(kind: &str, err: E) -> RetryError<opendal::Error> {
        let err = err.into();
        match err.kind() {
            opendal::ErrorKind::NotFound => {
                trace!("cache {kind} error (permanent): {err}");
                RetryError::permanent(err)
            }
            opendal::ErrorKind::PermissionDenied => {
                trace!("cache {kind} error (permanent): {err}");
                RetryError::permanent(err)
            }
            opendal::ErrorKind::RateLimited => {
                trace!("cache {kind} error (transient): {err}");
                RetryError::transient(err)
            }
            opendal::ErrorKind::Unsupported => {
                trace!("cache {kind} error (permanent): {err}");
                RetryError::permanent(err)
            }
            opendal::ErrorKind::Unexpected if err.is_temporary() => {
                debug!("cache {kind} error (transient): {err}");
                RetryError::transient(err)
            }
            _ => {
                warn!("cache {kind} error (permanent): {err:?}");
                RetryError::permanent(err)
            }
        }
    }

    pub async fn read_with_retry(
        storage: &opendal::Operator,
        key: &str,
    ) -> std::result::Result<opendal::Buffer, opendal::Error> {
        // TODO: Allow configuring the number of retries
        retry_with_jitter(usize::MAX, || async {
            storage
                .read(&normalize_key(key))
                .await
                .map_err(|e| to_retry_err("lookup", e))
        })
        .await
    }

    pub async fn write_with_retry(
        storage: &opendal::Operator,
        key: &str,
        buf: opendal::Buffer,
    ) -> std::result::Result<opendal::Metadata, opendal::Error> {
        // TODO: Allow configuring the number of retries
        retry_with_jitter(usize::MAX, || async {
            storage
                .write(&normalize_key(key), buf.clone())
                .await
                .map_err(|e| to_retry_err("write", e))
        })
        .await
    }
}

/// Wrapper for opendal::Operator that adds basedirs support
#[cfg(any(
    feature = "azure",
    feature = "gcs",
    feature = "gha",
    feature = "memcached",
    feature = "redis",
    feature = "s3",
    feature = "webdav",
    feature = "oss",
    feature = "cos"
))]
pub struct RemoteStorage {
    operator: opendal::Operator,
    basedirs: Vec<Vec<u8>>,
}

#[cfg(any(
    feature = "azure",
    feature = "gcs",
    feature = "gha",
    feature = "memcached",
    feature = "redis",
    feature = "s3",
    feature = "webdav",
    feature = "oss",
    feature = "cos"
))]
impl RemoteStorage {
    pub fn new(operator: opendal::Operator, basedirs: Vec<Vec<u8>>) -> Self {
        Self { operator, basedirs }
    }
}

/// Implement storage for operator.
#[cfg(any(
    feature = "azure",
    feature = "gcs",
    feature = "gha",
    feature = "memcached",
    feature = "redis",
    feature = "s3",
    feature = "webdav",
    feature = "oss",
    feature = "cos"
))]
#[async_trait]
impl Storage for RemoteStorage {
    async fn get(&self, key: &str) -> Result<Cache<opendal::Buffer>> {
        self.get_raw(key)
            .await
            .map(|data| data.map_or(Cache::Miss, Cache::Hit))
    }

    async fn del(&self, key: &str) -> Result<()> {
        self.operator
            .delete(&normalize_key(key))
            .await
            .map_err(anyhow::Error::new)
    }

    async fn has(&self, key: &str) -> bool {
        self.operator.stat(&normalize_key(key)).await.is_ok()
    }

    async fn put(&self, key: &str, entry: opendal::Buffer) -> Result<Duration> {
        trace!("RemoteStorage::put({key})");
        self.put_raw(key, entry).await
    }

    async fn size(&self, key: &str) -> Result<u64> {
        Ok(self
            .operator
            .stat_with(&normalize_key(key))
            .await?
            .content_length())
    }

    async fn check(&self) -> Result<CacheMode> {
        use opendal::ErrorKind;

        let path = ".sccache_check";

        // Read is required, return error directly if we can't read .
        match self.operator.read(path).await {
            Ok(_) => (),
            // Read not exist file with not found is ok.
            Err(err) if err.kind() == ErrorKind::NotFound => (),
            // Tricky Part.
            //
            // We tolerate rate limited here to make sccache keep running.
            // For the worse case, we will miss all the cache.
            //
            // In some super rare cases, user could configure storage in wrong
            // and hitting other services rate limit. There are few things we
            // can do, so we will print our the error here to make users know
            // about it.
            Err(err) if err.kind() == ErrorKind::RateLimited => {
                eprintln!("cache storage read check: {err:?}, but we decide to keep running");
            }
            Err(err) => bail!("cache storage failed to read: {err:?}"),
        }

        let can_write = match self.operator.write(path, "Hello, World!").await {
            Ok(_) => true,
            Err(err) if err.kind() == ErrorKind::AlreadyExists => true,
            // Tolerate all other write errors because we can do read at least.
            Err(err) => {
                eprintln!("storage write check failed: {err:?}");
                false
            }
        };

        let mode = if can_write {
            CacheMode::ReadWrite
        } else {
            CacheMode::ReadOnly
        };

        debug!("storage check result: {mode:?}");

        Ok(mode)
    }

    async fn location(&self) -> String {
        let meta = self.operator.info();
        format!(
            "{}, name: {}, prefix: {}",
            meta.scheme(),
            meta.name(),
            meta.root()
        )
    }

    fn cache_type_name(&self) -> &'static str {
        // Use opendal's scheme as the cache type name
        // This returns "s3", "redis", "azure", "gcs", etc.
        self.operator.info().scheme()
    }

    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        &self.basedirs
    }

    /// Get raw bytes from remote storage for multi-level backfill.
    ///
    /// Unlike `get()` which parses bytes into `CacheRead` (a `ZipArchive<Box<dyn ReadSeek>>`),
    /// this returns the raw bytes without parsing. `CacheRead` is a one-way transformation —
    /// there is no way to extract the original bytes back from the parsed ZIP archive.
    /// For backfill we need the raw bytes to write directly to another cache level.
    async fn get_raw(&self, key: &str) -> Result<Option<opendal::Buffer>> {
        trace!("opendal::Operator::get_raw({key})");
        match operator::read_with_retry(&self.operator, key).await {
            Ok(data) => {
                trace!(
                    "opendal::Operator::get_raw({key}): Found {} bytes",
                    data.len()
                );
                Ok(Some(data))
            }
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => {
                trace!("opendal::Operator::get_raw({key}): NotFound");
                Ok(None)
            }
            Err(e) => {
                warn!("opendal::Operator::get_raw({key}): Error: {e:?}");
                // Return error instead of silently returning None
                Err(anyhow!("Failed to read raw bytes: {e:?}"))
            }
        }
    }

    /// Write raw bytes to remote storage for multi-level backfill.
    ///
    /// Unlike `put()` which takes a `CacheWrite` and serializes it, this writes
    /// pre-serialized bytes directly. Paired with `get_raw()` for efficient
    /// level-to-level data transfer without a deserialize/reserialize round-trip.
    async fn put_raw(&self, key: &str, data: opendal::Buffer) -> Result<Duration> {
        trace!("opendal::Operator::put_raw({}, {} bytes)", key, data.len());
        let start = std::time::Instant::now();

        operator::write_with_retry(&self.operator, key, data).await?;

        Ok(start.elapsed())
    }
}

pub struct PreprocessorCache(pub Arc<dyn Storage>, pub PreprocessorCacheModeConfig);

impl PreprocessorCache {
    pub fn create(storage: Arc<dyn Storage>, cfg: PreprocessorCacheModeConfig) -> Arc<dyn Storage> {
        Arc::new(PreprocessorCache(storage, cfg))
    }
}

#[async_trait]
impl Storage for PreprocessorCache {
    async fn get(&self, key: &str) -> Result<Cache<opendal::Buffer>> {
        self.0.get(key).await
    }

    async fn del(&self, key: &str) -> Result<()> {
        self.0.del(key).await
    }

    async fn has(&self, key: &str) -> bool {
        self.0.has(key).await
    }

    async fn put(&self, key: &str, entry: opendal::Buffer) -> Result<Duration> {
        self.0.put(key, entry).await
    }

    async fn size(&self, key: &str) -> Result<u64> {
        self.0.size(key).await
    }

    async fn get_raw(&self, key: &str) -> Result<Option<opendal::Buffer>> {
        self.0.get_raw(key).await
    }

    async fn put_raw(&self, key: &str, entry: opendal::Buffer) -> Result<Duration> {
        self.0.put_raw(key, entry).await
    }

    /// Check the cache capability.
    async fn check(&self) -> Result<CacheMode> {
        self.0.check().await
    }

    /// Get the storage location.
    async fn location(&self) -> String {
        self.0.location().await
    }

    /// Get the cache backend type name.
    fn cache_type_name(&self) -> &'static str {
        self.0.cache_type_name()
    }

    /// Get the current storage usage, if applicable.
    async fn current_size(&self) -> Result<Option<u64>> {
        self.0.current_size().await
    }

    /// Get the maximum storage size, if applicable.
    async fn max_size(&self) -> Result<Option<u64>> {
        self.0.max_size().await
    }

    /// Return the config for preprocessor cache mode if applicable
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        self.1.clone()
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        self.0.basedirs()
    }
}

#[derive(Default)]
struct StorageBuilder {
    create_storage: Option<Box<dyn Fn() -> Result<Arc<dyn Storage>> + Send>>,
    preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    rw_mode: Option<CacheMode>,
    watch_paths: Vec<PathBuf>,
}

impl StorageBuilder {
    pub fn from_config(
        storage_kind: StorageKind,
        basedirs: Vec<Vec<u8>>,
        cache_type: CacheType,
    ) -> Result<StorageBuilder> {
        #[allow(unreachable_patterns)]
        match cache_type {
            CacheType::Azure(cfg) => {
                #[cfg(feature = "azure")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "azure"))]
                bail!("The 'azure' feature must be enabled to use the Azure storage backend")
            }
            CacheType::GCS(cfg) => {
                #[cfg(feature = "gcs")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "gcs"))]
                bail!("The 'gcs' feature must be enabled to use the GCS storage backend")
            }
            CacheType::GHA(cfg) => {
                #[cfg(feature = "gha")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "gha"))]
                bail!("The 'gha' feature must be enabled to use the GHA storage backend")
            }
            CacheType::Memcached(cfg) => {
                #[cfg(feature = "memcached")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "memcached"))]
                bail!(
                    "The 'memcached' feature must be enabled to use the Memcached storage backend"
                )
            }
            CacheType::Redis(cfg) => {
                #[cfg(feature = "redis")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "redis"))]
                bail!("The 'redis' feature must be enabled to use the Redis storage backend")
            }
            CacheType::S3(cfg) => {
                #[cfg(feature = "s3")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "s3"))]
                bail!("The 's3' feature must be enabled to use the S3 storage backend")
            }
            CacheType::Webdav(cfg) => {
                #[cfg(feature = "webdav")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "webdav"))]
                bail!("The 'webdav' feature must be enabled to use the Webdav storage backend")
            }
            CacheType::OSS(cfg) => {
                #[cfg(feature = "oss")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "oss"))]
                bail!("The 'oss' feature must be enabled to use the OSS storage backend")
            }
            CacheType::COS(cfg) => {
                #[cfg(feature = "cos")]
                return Ok((storage_kind, basedirs, cfg).into());
                #[cfg(not(feature = "cos"))]
                bail!("The 'cos' feature must be enabled to use the COS storage backend")
            }
            CacheType::Disk(cfg) => Ok((storage_kind, basedirs, cfg).into()),
            _ => Ok(StorageBuilder::default()),
        }
    }
    pub fn create_storage<F: Fn() -> Result<Arc<dyn Storage>> + Send + 'static>(
        self,
        factory: F,
    ) -> Self {
        Self {
            create_storage: Some(Box::new(factory)),
            ..self
        }
    }

    pub fn preprocessor_cache_mode<P: Into<Option<PreprocessorCacheModeConfig>>>(
        self,
        config: P,
    ) -> Self {
        Self {
            preprocessor_cache_mode: config.into(),
            ..self
        }
    }

    pub fn rw_mode<P: Into<Option<CacheMode>>>(self, mode: P) -> Self {
        Self {
            rw_mode: mode.into(),
            ..self
        }
    }

    pub fn watch_paths<P: ToOwned<Owned = Vec<PathBuf>>>(self, paths: P) -> Self {
        Self {
            watch_paths: paths.to_owned(),
            ..self
        }
    }

    pub async fn build(self) -> Result<Arc<dyn Storage>> {
        let Self {
            create_storage,
            preprocessor_cache_mode,
            rw_mode,
            #[allow(unused_variables)]
            watch_paths,
        } = self;

        let create_storage = create_storage.expect("create_storage should exist");

        let create_storage = move || {
            use futures::{FutureExt, TryFutureExt, future};
            future::ready(create_storage())
                .and_then(move |storage| async move {
                    // No need to check write if we are in manually-set read-only mode
                    if let Some(CacheMode::ReadOnly) = rw_mode {
                        Ok(ReadOnlyStorage::create(storage))
                    } else {
                        storage.check().await.map(|mode| match mode {
                            CacheMode::ReadWrite => storage,
                            CacheMode::ReadOnly => ReadOnlyStorage::create(storage),
                        })
                    }
                })
                .boxed()
        };

        #[cfg(not(feature = "watcher"))]
        let storage = create_storage().await?;
        #[cfg(feature = "watcher")]
        let storage = WatchStorage::from(create_storage, &watch_paths).await?;

        Ok(if let Some(cfg) = preprocessor_cache_mode {
            PreprocessorCache::create(storage.clone(), cfg)
        } else {
            storage
        })
    }
}

impl From<(StorageKind, Vec<Vec<u8>>, DiskCacheConfig)> for StorageBuilder {
    fn from(
        (storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, DiskCacheConfig),
    ) -> Self {
        let DiskCacheConfig {
            dir,
            size,
            rw_mode,
            preprocessor_cache_mode,
            ..
        } = config;

        let rw_mode = rw_mode.into();
        let dir = dir.join(storage_kind.key_prefix(String::new(), Some(&preprocessor_cache_mode)));

        Self::default()
            .create_storage(
                move || {
                    debug!("Init disk {storage_kind} cache with dir={dir:?}, size={size}, rw_mode={rw_mode:?}, basedirs={:?})", basedirs.iter().map(|b| String::from_utf8_lossy(b)).collect::<Vec<_>>());
                    Ok(Arc::new(DiskCache::new(&dir, size, rw_mode, basedirs.clone())))
                }
            )
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode))
    }
}

#[cfg(feature = "azure")]
impl From<(StorageKind, Vec<Vec<u8>>, AzureCacheConfig)> for StorageBuilder {
    fn from(
        (storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, AzureCacheConfig),
    ) -> Self {
        let AzureCacheConfig {
            connection_string,
            container,
            key_prefix,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!("Init azure {storage_kind} cache with container {container}, key_prefix {key_prefix}");

                AzureBlobCache::build(&connection_string, &container, &key_prefix)
                    .map(|storage| Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>)
                    .map_err(|err| anyhow!("create azure cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "gcs")]
impl From<(StorageKind, Vec<Vec<u8>>, GCSCacheConfig)> for StorageBuilder {
    fn from((storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, GCSCacheConfig)) -> Self {
        let GCSCacheConfig {
            bucket,
            key_prefix,
            cred_path,
            service_account,
            credential_url,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!(
                    "Init gcs {storage_kind} cache with bucket {bucket}, key_prefix {key_prefix}"
                );

                GCSCache::build(
                    &bucket,
                    &key_prefix,
                    cred_path.as_deref(),
                    service_account.as_deref(),
                    rw_mode.into(),
                    credential_url.as_deref(),
                )
                .map(|storage| {
                    Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>
                })
                .map_err(|err| anyhow!("create gcs cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "gha")]
impl From<(StorageKind, Vec<Vec<u8>>, GHACacheConfig)> for StorageBuilder {
    fn from((storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, GHACacheConfig)) -> Self {
        let GHACacheConfig {
            version,
            key_prefix,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!("Init gha {storage_kind} cache with version {version}");

                GHACache::build(&version, key_prefix.as_str())
                    .map(|storage| {
                        Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>
                    })
                    .map_err(|err| anyhow!("create gha cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "memcached")]
impl From<(StorageKind, Vec<Vec<u8>>, MemcachedCacheConfig)> for StorageBuilder {
    fn from(
        (storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, MemcachedCacheConfig),
    ) -> Self {
        let MemcachedCacheConfig {
            url,
            username,
            password,
            expiration,
            connection_pool_max_size,
            key_prefix,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let connection_pool_max_size = connection_pool_max_size.unwrap_or(10);
        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!("Init memcached {storage_kind} cache with url {url}");

                MemcachedCache::build(
                    &url,
                    username.as_deref(),
                    password.as_deref(),
                    &key_prefix,
                    expiration,
                    connection_pool_max_size,
                )
                .map(|storage| {
                    Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>
                })
                .map_err(|err| anyhow!("create memcached cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "redis")]
impl From<(StorageKind, Vec<Vec<u8>>, RedisCacheConfig)> for StorageBuilder {
    fn from(
        (storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, RedisCacheConfig),
    ) -> Self {
        use crate::cache::simplex::SimplexCache;

        let RedisCacheConfig {
            endpoint,
            cluster_endpoints,
            reader_endpoints,
            username,
            password,
            db,
            url,
            ttl,
            connection_pool_max_size,
            key_prefix,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let connection_pool_max_size = connection_pool_max_size.unwrap_or(10);
        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                match (&endpoint, &cluster_endpoints, &url) {
                    (Some(url), None, None) => {
                        debug!("Init redis single-node {storage_kind} cache with url {url}");
                        RedisCache::build_single(
                            url,
                            username.as_deref(),
                            password.as_deref(),
                            db,
                            &key_prefix,
                            ttl,
                            connection_pool_max_size,
                        )
                    }
                    (None, Some(urls), None) => {
                        debug!("Init redis cluster {storage_kind} cache with urls {urls}");
                        RedisCache::build_cluster(
                            urls,
                            username.as_deref(),
                            password.as_deref(),
                            db,
                            &key_prefix,
                            ttl,
                            connection_pool_max_size,
                        )
                    }
                    (None, None, Some(url)) => {
                        warn!("Init redis single-node {storage_kind} cache from deprecated API with url {url}");
                        if username.is_some() || password.is_some() || db != DEFAULT_REDIS_DB {
                            bail!("`username`, `password` and `db` has no effect when `url` is set. Please use `endpoint` or `cluster_endpoints` for new API accessing");
                        }

                        RedisCache::build_from_url(url, &key_prefix, ttl, connection_pool_max_size)
                    }
                    _ => bail!("Only one of `endpoint`, `cluster_endpoints`, `url` must be set"),
                }
                .and_then(|storage| {
                    if let Some(reader_endpoints) = &reader_endpoints {
                        debug!("Init redis cluster {storage_kind} cache with reader endpoints {reader_endpoints}");
                        let reader = if reader_endpoints.contains(",") {
                            RedisCache::build_cluster(
                                reader_endpoints,
                                username.as_deref(),
                                password.as_deref(),
                                db,
                                &key_prefix,
                                ttl,
                                connection_pool_max_size,
                            )?
                        } else {
                            RedisCache::build_single(
                                reader_endpoints,
                                username.as_deref(),
                                password.as_deref(),
                                db,
                                &key_prefix,
                                ttl,
                                connection_pool_max_size,
                            )?
                        };
                        let reader = Arc::new(RemoteStorage::new(reader, basedirs.clone()));
                        let storage = Arc::new(RemoteStorage::new(storage, basedirs.clone()));
                        Ok(SimplexCache::create(reader, storage))
                    } else {
                        Ok(Arc::new(RemoteStorage::new(storage, basedirs.clone())))
                    }
                })
                .map_err(|err| anyhow!("create redis cache failed: {err:?}"))
        })
        .preprocessor_cache_mode(preprocessor_cache_mode)
        .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "s3")]
impl From<(StorageKind, Vec<Vec<u8>>, S3CacheConfig)> for StorageBuilder {
    fn from((storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, S3CacheConfig)) -> Self {
        let S3CacheConfig {
            bucket,
            enable_virtual_host_style,
            endpoint,
            key_prefix,
            no_credentials,
            region,
            server_side_encryption,
            use_ssl,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
            debug!("Init s3 {storage_kind} cache with endpoint {endpoint:?}, bucket {bucket:?}, and key prefix: {key_prefix:?}");

            S3Cache::new(bucket.clone(), key_prefix.clone(), no_credentials)
                .with_region(region.clone())
                .with_endpoint(endpoint.clone())
                .with_use_ssl(use_ssl)
                .with_server_side_encryption(server_side_encryption)
                .with_enable_virtual_host_style(enable_virtual_host_style)
                .build()
                .map(|storage| Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>)
                .map_err(|err| anyhow!("create s3 cache failed: {err:?}"))
        })
        .preprocessor_cache_mode(preprocessor_cache_mode)
        .rw_mode(Some(rw_mode.into()))
        .watch_paths({
            let ctx = reqsign::default_context();
            [
                ctx.env_var("AWS_CONFIG_FILE").unwrap_or_else(|| "~/.aws/config".into()),
                ctx.env_var("AWS_SHARED_CREDENTIALS_FILE").unwrap_or_else(|| "~/.aws/credentials".into()),
            ]
            .iter()
            .filter_map(|s| ctx.expand_home_dir(s))
            .map(PathBuf::from)
            .collect::<Vec<_>>()
        })
    }
}

#[cfg(feature = "webdav")]
impl From<(StorageKind, Vec<Vec<u8>>, WebdavCacheConfig)> for StorageBuilder {
    fn from(
        (storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, WebdavCacheConfig),
    ) -> Self {
        let WebdavCacheConfig {
            endpoint,
            key_prefix,
            password,
            token,
            username,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!("Init webdav {storage_kind} cache with endpoint {endpoint}");

                WebdavCache::build(
                    &endpoint,
                    &key_prefix,
                    username.as_deref(),
                    password.as_deref(),
                    token.as_deref(),
                )
                .map(|storage| {
                    Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>
                })
                .map_err(|err| anyhow!("create webdav cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "oss")]
impl From<(StorageKind, Vec<Vec<u8>>, OSSCacheConfig)> for StorageBuilder {
    fn from((storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, OSSCacheConfig)) -> Self {
        let OSSCacheConfig {
            bucket,
            endpoint,
            key_prefix,
            no_credentials,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!("Init oss {storage_kind} cache with bucket {bucket}, endpoint {endpoint:?}");

                OSSCache::build(&bucket, &key_prefix, endpoint.as_deref(), no_credentials)
                    .map(|storage| {
                        Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>
                    })
                    .map_err(|err| anyhow!("create oss cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[cfg(feature = "cos")]
impl From<(StorageKind, Vec<Vec<u8>>, COSCacheConfig)> for StorageBuilder {
    fn from((storage_kind, basedirs, config): (StorageKind, Vec<Vec<u8>>, COSCacheConfig)) -> Self {
        let COSCacheConfig {
            bucket,
            endpoint,
            key_prefix,
            preprocessor_cache_mode,
            rw_mode,
            ..
        } = config;

        let key_prefix = storage_kind.key_prefix(key_prefix, preprocessor_cache_mode.as_ref());

        Self::default()
            .create_storage(move || {
                debug!("Init cos {storage_kind} cache with bucket {bucket}, endpoint {endpoint:?}");

                COSCache::build(&bucket, &key_prefix, endpoint.as_deref())
                    .map(|storage| {
                        Arc::new(RemoteStorage::new(storage, basedirs.clone())) as Arc<dyn Storage>
                    })
                    .map_err(|err| anyhow!("create oss cache failed: {err:?}"))
            })
            .preprocessor_cache_mode(preprocessor_cache_mode)
            .rw_mode(Some(rw_mode.into()))
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum StorageKind {
    Compilations,
    Preprocessor,
}

impl fmt::Display for StorageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compilations => write!(f, "object"),
            Self::Preprocessor => write!(f, "preprocessor"),
        }
    }
}

impl StorageKind {
    /// Create suitable `Storage` implementations from a list of storage configurations.
    pub async fn create(
        &self,
        caches: &CacheConfigs,
        basedirs: &[Vec<u8>],
    ) -> Result<Arc<dyn Storage>> {
        let kind = *self;

        let cache_types: Result<Vec<CacheType>> = caches.into();
        let multilevel = caches.multilevel.as_ref();
        let levels = multilevel.map(|multilevel| {
            multilevel
                .chain
                .iter()
                .map(|level| level.trim().to_lowercase())
                .collect::<Vec<_>>()
        });

        let storage = futures::stream::iter(
            cache_types?
                .iter()
                .filter({
                    let use_preprocessor_cache_mode = matches!(kind, StorageKind::Preprocessor);
                    move |&cache| {
                        if use_preprocessor_cache_mode {
                            match cache {
                                CacheType::Disk(_) => true,
                                _ => cache
                                    .preprocessor_cache_mode()
                                    .map(|cache| cache.use_preprocessor_cache_mode)
                                    .filter(|&f| f == use_preprocessor_cache_mode)
                                    .unwrap_or_default(),
                            }
                        } else {
                            true
                        }
                    }
                })
                .map(|cache| {
                    let level = Into::<&str>::into(cache).to_owned();

                    if let Some(levels) = levels.as_ref() && !levels.contains(&level) {
                        return Err(anyhow!(
                            "Cache level '{level}' specified in SCCACHE_MULTILEVEL_CHAIN but not configured (missing environment variables)"
                        ))
                    }
                    StorageBuilder::from_config(kind, basedirs.to_vec(), cache.clone())
                }),
        )
        .and_then(|builder| builder.build().boxed())
        .inspect_err(|err| error!("storage init failed for {kind} cache: {err:?}"))
        .try_collect::<Vec<_>>()
        .await?;

        let storage = if storage.len() == 1 {
            storage[0].clone()
        } else if storage.len() > 1 {
            Arc::new(
                MultiLevelStorage::with_write_error_policy(
                    storage,
                    multilevel
                        .map(|multilevel| multilevel.write_error_policy)
                        .unwrap_or_default(),
                )
                .await,
            )
        } else if let Some(levels) = levels.as_ref().filter(|levels| !levels.is_empty()) {
            return Err(anyhow!(
                "Multi-level cache configured with {} levels but none could be built",
                levels.len()
            ));
        } else {
            // If we build only with `cargo build --no-default-features`
            // only use sccache with a local cache (no remote storage)
            StorageBuilder::from((kind, basedirs.to_vec(), DiskCacheConfig::default()))
                .build()
                .await?
        };

        storage
            .check()
            .await
            .context("storage check failed")
            .map(|mode| {
                info!("server configured with {kind} cache: {mode:?}");
                storage
            })
    }

    fn key_prefix(
        &self,
        key_prefix: String,
        preprocessor_cache_mode: Option<&PreprocessorCacheModeConfig>,
    ) -> String {
        preprocessor_cache_mode
            .filter(|&p| p.use_preprocessor_cache_mode)
            .filter(|_| matches!(self, StorageKind::Preprocessor))
            .map(|p| p.key_prefix.clone())
            .unwrap_or(key_prefix)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::config::{CacheModeConfig, Config};
    use fs_err as fs;

    #[test]
    fn test_read_write_mode_local() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        // Use disk cache.
        let tempdir = crate::util::temp_dir()
            .context("Failed to create tempdir")
            .unwrap();

        let cache_dir = tempdir.path().join("cache");
        fs::create_dir(&cache_dir).unwrap();

        let make_config = |rw_mode| Config {
            caches: CacheConfigs {
                disk: Some(DiskCacheConfig {
                    dir: cache_dir.clone(),
                    rw_mode,
                    ..DiskCacheConfig::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        // Test Read Write
        {
            let caches = make_config(CacheModeConfig::ReadWrite).caches;
            runtime.block_on(async {
                let storage = StorageKind::Compilations
                    .create(&caches, &[])
                    .await
                    .unwrap();
                storage.put("test1", "entry".into()).await.unwrap();
            });
        }

        // Test Read-only
        {
            let caches = make_config(CacheModeConfig::ReadOnly).caches;
            runtime.block_on(async {
                let storage = StorageKind::Compilations
                    .create(&caches, &[])
                    .await
                    .unwrap();
                assert_eq!(
                    storage
                        .put("test1", "entry".into())
                        .await
                        .unwrap_err()
                        .to_string(),
                    "Cannot write to read-only storage"
                );
            });
        }
    }

    #[test]
    #[cfg(feature = "s3")]
    fn test_operator_storage_s3_with_basedirs() {
        // Create S3 operator (doesn't need real credentials for this test)
        let operator = crate::cache::s3::S3Cache::new(
            "test-bucket".to_string(),
            "test-prefix".to_string(),
            true, // no_credentials = true
        )
        .with_region(Some("us-east-1".to_string()))
        .build()
        .expect("Failed to create S3 cache operator");

        let basedirs = vec![b"/home/user/project".to_vec(), b"/opt/build".to_vec()];

        // Wrap with RemoteStorage
        let storage = RemoteStorage::new(operator, basedirs.clone());

        // Verify basedirs are stored and retrieved correctly
        let basedirs_actual = storage.basedirs();
        assert_eq!(basedirs_actual, basedirs.as_slice());
        assert_eq!(basedirs_actual.len(), 2);
        assert_eq!(basedirs_actual[0], b"/home/user/project".to_vec());
        assert_eq!(basedirs_actual[1], b"/opt/build".to_vec());
    }

    #[test]
    #[cfg(feature = "redis")]
    fn test_operator_storage_redis_with_basedirs() {
        // Create Redis operator
        let operator = crate::cache::redis::RedisCache::build_single(
            "redis://localhost:6379",
            None,
            None,
            0,
            "test-prefix",
            0,
            10,
        )
        .expect("Failed to create Redis cache operator");

        let basedirs = vec![b"/workspace".to_vec()];

        // Wrap with RemoteStorage
        let storage = RemoteStorage::new(operator, basedirs.clone());

        // Verify basedirs work
        let basedirs_actual = storage.basedirs();
        assert_eq!(basedirs_actual, basedirs.as_slice());
        assert_eq!(basedirs_actual.len(), 1);
    }

    #[test]
    #[cfg(feature = "redis")]
    fn test_operator_storage_redis_with_read_only() {
        // Create Redis operator

        use crate::test::utils::Waiter;
        let operator = crate::cache::redis::RedisCache::build_single(
            "redis://localhost:6379",
            None,
            None,
            0,
            "test-prefix",
            0,
            10,
        )
        .expect("Failed to create Redis cache operator");

        // Wrap with ReadOnlyStorage(RemoteStorage)
        let storage = ReadOnlyStorage::create(Arc::new(RemoteStorage::new(operator, vec![])));

        // Verify put fails
        let result = storage.put("test", opendal::Buffer::new()).wait();
        match result {
            Ok(_) => panic!("expected error, got success {result:?}"),
            Err(err) => assert!(err.to_string().contains("read-only")),
        }
    }
}
