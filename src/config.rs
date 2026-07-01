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

use crate::cache::CacheMode;
#[cfg(target_os = "windows")]
use crate::util::normalize_win_path;
use directories::ProjectDirs;
use fs::File;
use fs_err as fs;
use itertools::Itertools;
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
use serde::ser::Serializer;
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeOwned, Deserializer},
};
#[cfg(test)]
use serial_test::serial;
use std::{
    collections::HashMap,
    env, fmt,
    io::{Read, Write},
    path::{Path, PathBuf},
    result::Result as StdResult,
    str::FromStr,
    sync::{LazyLock, Mutex},
    time::Duration,
};
use typed_path::Utf8TypedPathBuf;

use crate::errors::*;

/// Defines how the multi-level cache handles write failures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteErrorPolicy {
    /// Never fail on write errors - log warnings only (most permissive)
    Ignore,
    /// Fail only if L0 write fails (default - balances reliability and performance)
    #[default]
    L0,
    /// Fail if any read-write level fails (most strict)
    All,
}

impl FromStr for WriteErrorPolicy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "ignore" => Ok(WriteErrorPolicy::Ignore),
            "l0" => Ok(WriteErrorPolicy::L0),
            "all" => Ok(WriteErrorPolicy::All),
            _ => Err(anyhow!(
                "Invalid write policy '{s}'. Valid values: ignore, l0, all"
            )),
        }
    }
}

impl fmt::Display for WriteErrorPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WriteErrorPolicy::Ignore => write!(f, "ignore"),
            WriteErrorPolicy::L0 => write!(f, "l0"),
            WriteErrorPolicy::All => write!(f, "all"),
        }
    }
}

/// Configuration for multi-level cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiLevelConfig {
    /// Ordered list of cache backends (L0, L1, L2, ...)
    #[serde(rename = "chain")]
    pub chain: Vec<String>,
    /// Write failure handling policy
    #[serde(default)]
    pub write_error_policy: WriteErrorPolicy,
}

static CACHED_CONFIG_PATH: LazyLock<PathBuf> = LazyLock::new(CachedConfig::file_config_path);
static CACHED_CONFIG: Mutex<Option<CachedFileConfig>> = Mutex::new(None);

const ORGANIZATION: &str = "Mozilla";
const APP_NAME: &str = "sccache";
const DIST_APP_NAME: &str = "sccache-dist-client";
const TEN_GIGS: u64 = 10 * 1024 * 1024 * 1024;

pub const INSECURE_DIST_CLIENT_TOKEN: &str = "dangerously_insecure_client";

// Unfortunately this means that nothing else can use the sccache cache dir as
// this top level directory is used directly to store sccache cached objects...
pub fn default_disk_cache_dir() -> PathBuf {
    ProjectDirs::from("", ORGANIZATION, APP_NAME)
        .expect("Unable to retrieve disk cache directory")
        .cache_dir()
        .to_owned()
}
// ...whereas subdirectories are used of this one
pub fn default_dist_cache_dir() -> PathBuf {
    ProjectDirs::from("", ORGANIZATION, DIST_APP_NAME)
        .expect("Unable to retrieve dist cache directory")
        .cache_dir()
        .to_owned()
}

fn default_disk_cache_size() -> u64 {
    TEN_GIGS
}
fn default_toolchain_cache_size() -> u64 {
    TEN_GIGS
}

struct StringOrU64Visitor;

impl de::Visitor<'_> for StringOrU64Visitor {
    type Value = u64;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a string with size suffix (like '20G') or a u64")
    }

    fn visit_str<E>(self, value: &str) -> StdResult<Self::Value, E>
    where
        E: de::Error,
    {
        parse_size(value).ok_or_else(|| E::custom(format!("Invalid size value: {value}")))
    }

    fn visit_u64<E>(self, value: u64) -> StdResult<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value)
    }

    fn visit_i64<E>(self, value: i64) -> StdResult<Self::Value, E>
    where
        E: de::Error,
    {
        if value < 0 {
            Err(E::custom("negative values not supported"))
        } else {
            Ok(value as u64)
        }
    }
}

fn deserialize_size_from_str<'de, D>(deserializer: D) -> StdResult<u64, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_any(StringOrU64Visitor)
}

pub fn parse_size(val: &str) -> Option<u64> {
    let multiplier = match val.chars().last().map(|v| v.to_ascii_uppercase()) {
        Some('K') => 1024,
        Some('M') => 1024 * 1024,
        Some('G') => 1024 * 1024 * 1024,
        Some('T') => 1024 * 1024 * 1024 * 1024,
        _ => 1,
    };
    let val = if multiplier > 1 && !val.is_empty() {
        val.split_at(val.len() - 1).0
    } else {
        val
    };
    u64::from_str(val).ok().map(|size| size * multiplier)
}

#[cfg(any(feature = "dist-client", feature = "dist-server"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HTTPUrl(reqwest::Url);
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
impl Serialize for HTTPUrl {
    fn serialize<S>(&self, serializer: S) -> StdResult<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.0.as_str())
    }
}
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
impl<'a> Deserialize<'a> for HTTPUrl {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        use serde::de::Error;
        let helper: String = Deserialize::deserialize(deserializer)?;
        let url = parse_http_url(helper).map_err(D::Error::custom)?;
        Ok(HTTPUrl(url))
    }
}
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
fn parse_http_url<K: AsRef<str>>(str: K) -> Result<reqwest::Url> {
    use std::net::SocketAddr;
    let url = str.as_ref();
    let url = if let Ok(sa) = url.parse::<SocketAddr>() {
        warn!("Url {url} has no scheme, assuming http");
        reqwest::Url::parse(&format!("http://{sa}"))
    } else {
        reqwest::Url::parse(url)
    }?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("url not http or https")
    }
    // TODO: relative url handling just hasn't been implemented and tested
    if url.path() != "/" {
        bail!("url has a relative path (currently unsupported)")
    }
    Ok(url)
}
#[cfg(any(feature = "dist-client", feature = "dist-server"))]
impl HTTPUrl {
    pub fn from_url(u: reqwest::Url) -> Self {
        HTTPUrl(u)
    }
    pub fn to_url(&self) -> reqwest::Url {
        self.0.clone()
    }
}

enum MultiLevelCacheOrder {
    Disk = 0,
    S3 = 1,
    Redis = 2,
    Memcached = 3,
    Gcs = 4,
    Gha = 5,
    Azure = 6,
    Webdav = 7,
    Oss = 8,
    Cos = 9,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AzureCacheConfig {
    #[serde(default)]
    pub connection_string: String,
    #[serde(default)]
    pub container: String,
    #[serde(default)]
    pub key_prefix: String,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "AzureCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for AzureCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl AzureCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Azure as u64
    }
}

/// Configuration switches for preprocessor cache mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct PreprocessorCacheModeConfig {
    /// Whether to use preprocessor cache mode entirely
    pub use_preprocessor_cache_mode: bool,
    /// The prefix in cache storage where the preprocessor cache files are stored.
    /// For example, the disk preprocessor cache will be `$SCCACHE_DIR/{key_prefix}`,
    /// the S3 preprocessor cache files will be under `$SCCACHE_BUCKET/{key_prefix}`,
    /// etc.
    pub key_prefix: String,
}

impl Default for PreprocessorCacheModeConfig {
    fn default() -> Self {
        Self {
            use_preprocessor_cache_mode: false,
            key_prefix: "preprocessor".into(),
        }
    }
}

impl PreprocessorCacheModeConfig {
    /// Return a default [`Self`], but with the cache active.
    pub fn activated() -> Self {
        Self {
            use_preprocessor_cache_mode: true,
            ..Default::default()
        }
    }

    pub fn default_key_prefix() -> String {
        "preprocessor".into()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(default)]
pub struct DiskCacheConfig {
    pub dir: PathBuf,
    #[serde(deserialize_with = "deserialize_size_from_str")]
    pub size: u64,
    pub preprocessor_cache_mode: PreprocessorCacheModeConfig,
    #[serde(default = "DiskCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl DiskCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Disk as u64
    }
}

impl Default for DiskCacheConfig {
    fn default() -> Self {
        DiskCacheConfig {
            dir: default_disk_cache_dir(),
            size: default_disk_cache_size(),
            preprocessor_cache_mode: PreprocessorCacheModeConfig::activated(),
            rw_mode: CacheModeConfig::ReadWrite,
            order: DiskCacheConfig::default_order(),
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum CacheModeConfig {
    #[serde(rename = "READ_ONLY")]
    ReadOnly,
    #[serde(rename = "READ_WRITE")]
    #[default]
    ReadWrite,
}

impl From<CacheModeConfig> for CacheMode {
    fn from(value: CacheModeConfig) -> Self {
        match value {
            CacheModeConfig::ReadOnly => CacheMode::ReadOnly,
            CacheModeConfig::ReadWrite => CacheMode::ReadWrite,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GCSCacheConfig {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub key_prefix: String,
    pub cred_path: Option<String>,
    pub service_account: Option<String>,
    pub credential_url: Option<String>,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "GCSCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for GCSCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl GCSCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Gcs as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GHACacheConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub key_prefix: String,
    /// Version for gha cache is a namespace. By setting different versions,
    /// we can avoid mixed caches.
    #[serde(default)]
    pub version: String,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "GHACacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for GHACacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl GHACacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Gha as u64
    }
}

/// Memcached's default value of expiration is 10800s (3 hours), which is too
/// short for use case of sccache.
///
/// We increase the default expiration to 86400s (1 day) to balance between
/// memory consumpation and cache hit rate.
///
/// Please change this value freely if we have a better choice.
const DEFAULT_MEMCACHED_CACHE_EXPIRATION: u32 = 86400;

fn default_memcached_cache_expiration() -> u32 {
    DEFAULT_MEMCACHED_CACHE_EXPIRATION
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemcachedCacheConfig {
    #[serde(default)]
    #[serde(alias = "endpoint")]
    pub url: String,

    /// Username to authenticate with.
    pub username: Option<String>,

    /// Password to authenticate with.
    pub password: Option<String>,

    /// the expiration time in seconds.
    ///
    /// Default to 24 hours (86400)
    /// Up to 30 days (2592000)
    #[serde(default = "default_memcached_cache_expiration")]
    pub expiration: u32,

    #[serde(default)]
    pub key_prefix: String,

    /// The maximum number of connections allowed.
    ///
    /// Default to 10
    pub connection_pool_max_size: Option<u32>,

    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,

    #[serde(default = "MemcachedCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for MemcachedCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl MemcachedCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Memcached as u64
    }
}

/// redis has no default TTL - all caches live forever
///
/// We keep the TTL as 0 here as redis does
///
/// Please change this value freely if we have a better choice.
const DEFAULT_REDIS_CACHE_TTL: u64 = 0;
pub const DEFAULT_REDIS_DB: u32 = 0;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisCacheConfig {
    /// The single-node redis endpoint.
    /// Mutually exclusive with `cluster_endpoints`.
    pub endpoint: Option<String>,

    /// The redis cluster endpoints.
    /// Mutually exclusive with `endpoint`.
    pub cluster_endpoints: Option<String>,

    /// The redis reader endpoint(s).
    pub reader_endpoints: Option<String>,

    /// Username to authenticate with.
    pub username: Option<String>,

    /// Password to authenticate with.
    pub password: Option<String>,

    /// The redis URL.
    /// Deprecated in favor of `endpoint`.
    pub url: Option<String>,

    /// the db number to use
    ///
    /// Default to 0
    #[serde(default)]
    pub db: u32,

    /// the ttl (expiration) time in seconds.
    ///
    /// Default to infinity (0)
    #[serde(default, alias = "expiration")]
    pub ttl: u64,

    /// The maximum number of connections allowed.
    ///
    /// Default to 10
    pub connection_pool_max_size: Option<u32>,

    #[serde(default)]
    pub key_prefix: String,

    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,

    #[serde(default = "RedisCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for RedisCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl RedisCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Redis as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebdavCacheConfig {
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub key_prefix: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "WebdavCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for WebdavCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl WebdavCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Webdav as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3CacheConfig {
    #[serde(default)]
    pub bucket: String,
    pub region: Option<String>,
    #[serde(default)]
    pub key_prefix: String,
    #[serde(default)]
    pub no_credentials: bool,
    pub endpoint: Option<String>,
    pub use_ssl: Option<bool>,
    pub server_side_encryption: Option<bool>,
    pub enable_virtual_host_style: Option<bool>,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "S3CacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for S3CacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl S3CacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::S3 as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OSSCacheConfig {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub key_prefix: String,
    pub endpoint: Option<String>,
    #[serde(default)]
    pub no_credentials: bool,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "OSSCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for OSSCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl OSSCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Oss as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct COSCacheConfig {
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub key_prefix: String,
    pub endpoint: Option<String>,
    pub preprocessor_cache_mode: Option<PreprocessorCacheModeConfig>,
    #[serde(default = "COSCacheConfig::default_order")]
    pub order: u64,
    #[serde(default)]
    pub rw_mode: CacheModeConfig,
}

impl Default for COSCacheConfig {
    fn default() -> Self {
        serde_json::from_str("{}").unwrap()
    }
}

impl COSCacheConfig {
    fn default_order() -> u64 {
        MultiLevelCacheOrder::Cos as u64
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CacheType {
    Azure(AzureCacheConfig),
    Disk(DiskCacheConfig),
    GCS(GCSCacheConfig),
    GHA(GHACacheConfig),
    Memcached(MemcachedCacheConfig),
    Redis(RedisCacheConfig),
    S3(S3CacheConfig),
    Webdav(WebdavCacheConfig),
    OSS(OSSCacheConfig),
    COS(COSCacheConfig),
}

impl CacheType {
    pub fn order(&self) -> u64 {
        match self {
            Self::Azure(cfg) => cfg.order,
            Self::Disk(cfg) => cfg.order,
            Self::GCS(cfg) => cfg.order,
            Self::GHA(cfg) => cfg.order,
            Self::Memcached(cfg) => cfg.order,
            Self::Redis(cfg) => cfg.order,
            Self::S3(cfg) => cfg.order,
            Self::Webdav(cfg) => cfg.order,
            Self::OSS(cfg) => cfg.order,
            Self::COS(cfg) => cfg.order,
        }
    }
    pub fn preprocessor_cache_mode(&self) -> Option<PreprocessorCacheModeConfig> {
        match self {
            Self::Azure(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::Disk(cfg) => Some(cfg.preprocessor_cache_mode.clone()),
            Self::GCS(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::GHA(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::Memcached(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::Redis(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::S3(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::Webdav(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::OSS(cfg) => cfg.preprocessor_cache_mode.clone(),
            Self::COS(cfg) => cfg.preprocessor_cache_mode.clone(),
        }
    }
}

impl From<&CacheType> for &str {
    fn from(cache: &CacheType) -> Self {
        match cache {
            CacheType::Disk(_) => "disk",
            CacheType::S3(_) => "s3",
            CacheType::Redis(_) => "redis",
            CacheType::Memcached(_) => "memcached",
            CacheType::GCS(_) => "gcs",
            CacheType::GHA(_) => "gha",
            CacheType::Azure(_) => "azure",
            CacheType::Webdav(_) => "webdav",
            CacheType::OSS(_) => "oss",
            CacheType::COS(_) => "cos",
        }
    }
}

impl PartialOrd for CacheType {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CacheType {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order().cmp(&other.order())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfigs {
    pub azure: Option<AzureCacheConfig>,
    pub disk: Option<DiskCacheConfig>,
    pub gcs: Option<GCSCacheConfig>,
    pub gha: Option<GHACacheConfig>,
    pub memcached: Option<MemcachedCacheConfig>,
    pub redis: Option<RedisCacheConfig>,
    pub s3: Option<S3CacheConfig>,
    pub webdav: Option<WebdavCacheConfig>,
    pub oss: Option<OSSCacheConfig>,
    pub cos: Option<COSCacheConfig>,
    /// Multi-level cache configuration
    pub multilevel: Option<MultiLevelConfig>,
}

impl CacheConfigs {
    /// Override self with any existing fields from other
    fn merge(mut self, other: Self) -> Self {
        let CacheConfigs {
            azure,
            disk,
            gcs,
            gha,
            memcached,
            redis,
            s3,
            webdav,
            oss,
            cos,
            multilevel,
        } = other;

        if azure.is_some() {
            self.azure = azure;
        }
        if disk.is_some() {
            self.disk = disk;
        }
        if gcs.is_some() {
            self.gcs = gcs;
        }
        if gha.is_some() {
            self.gha = gha;
        }
        if memcached.is_some() {
            self.memcached = memcached;
        }
        if redis.is_some() {
            self.redis = redis;
        }
        if s3.is_some() {
            self.s3 = s3;
        }
        if webdav.is_some() {
            self.webdav = webdav;
        }
        if oss.is_some() {
            self.oss = oss;
        }
        if cos.is_some() {
            self.cos = cos;
        }
        if multilevel.is_some() {
            self.multilevel = multilevel.map(|mut multilevel| {
                for level in multilevel.chain.as_mut_slice() {
                    *level = level.trim().to_lowercase();
                }
                multilevel
            });
        }

        self
    }
}

// Return a list of caches in the order determined by the user
impl From<&CacheConfigs> for Result<Vec<CacheType>> {
    fn from(caches: &CacheConfigs) -> Self {
        let chain = caches.multilevel.as_ref().map(|multilevel| {
            multilevel
                .chain
                .iter()
                .map(|level| level.as_str())
                .map(|level| match level {
                    "disk" => Ok(level),
                    "s3" => Ok(level),
                    "redis" => Ok(level),
                    "memcached" => Ok(level),
                    "gcs" => Ok(level),
                    "gha" => Ok(level),
                    "azure" => Ok(level),
                    "webdav" => Ok(level),
                    "oss" => Ok(level),
                    "cos" => Ok(level),
                    _ => Err(anyhow!("Unknown cache level: {level:?}")),
                })
                .try_collect::<_, Vec<_>, _>()
        });

        if let Some(chain) = chain {
            return chain?
                .into_iter()
                .map(|level| {
                    match level {
                        "disk" => caches.disk.clone().map(CacheType::Disk).map(Ok),
                        "s3" => caches.s3.clone().map(CacheType::S3).map(Ok),
                        "redis" => caches.redis.clone().map(CacheType::Redis).map(Ok),
                        "memcached" => caches.memcached.clone().map(CacheType::Memcached).map(Ok),
                        "gcs" => caches.gcs.clone().map(CacheType::GCS).map(Ok),
                        "gha" => caches.gha.clone().map(CacheType::GHA).map(Ok),
                        "azure" => caches.azure.clone().map(CacheType::Azure).map(Ok),
                        "webdav" => caches.webdav.clone().map(CacheType::Webdav).map(Ok),
                        "oss" => caches.oss.clone().map(CacheType::OSS).map(Ok),
                        "cos" => caches.cos.clone().map(CacheType::COS).map(Ok),
                        _ => Some(Err(anyhow!("Unknown cache level: {level:?}"))),
                    }
                    .ok_or_else(|| anyhow!("{level} cache not configured but specified in levels"))
                })
                .flatten_ok()
                .try_collect();
        }

        Ok([
            caches.disk.clone().map(CacheType::Disk),
            caches.azure.clone().map(CacheType::Azure),
            caches.gcs.clone().map(CacheType::GCS),
            caches.gha.clone().map(CacheType::GHA),
            caches.memcached.clone().map(CacheType::Memcached),
            caches.redis.clone().map(CacheType::Redis),
            caches.s3.clone().map(CacheType::S3),
            caches.webdav.clone().map(CacheType::Webdav),
            caches.oss.clone().map(CacheType::OSS),
            caches.cos.clone().map(CacheType::COS),
        ]
        .into_iter()
        .flatten()
        // Default sort order
        .sorted()
        .collect::<Vec<_>>())
    }
}

impl From<CacheType> for CacheConfigs {
    fn from(cache: CacheType) -> Self {
        let mut caches = Self {
            multilevel: Some(MultiLevelConfig {
                chain: vec![Into::<&str>::into(&cache).into()],
                write_error_policy: Default::default(),
            }),
            ..Self::default()
        };

        match cache {
            CacheType::Azure(c) => caches.azure = Some(c),
            CacheType::Disk(c) => caches.disk = Some(c),
            CacheType::GCS(c) => caches.gcs = Some(c),
            CacheType::GHA(c) => caches.gha = Some(c),
            CacheType::Memcached(c) => caches.memcached = Some(c),
            CacheType::Redis(c) => caches.redis = Some(c),
            CacheType::S3(c) => caches.s3 = Some(c),
            CacheType::Webdav(c) => caches.webdav = Some(c),
            CacheType::OSS(c) => caches.oss = Some(c),
            CacheType::COS(c) => caches.cos = Some(c),
        }

        caches
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(tag = "type")]
pub enum DistToolchainConfig {
    #[serde(rename = "no_dist")]
    NoDist { compiler_executable: PathBuf },
    #[serde(rename = "path_override")]
    PathOverride {
        compiler_executable: PathBuf,
        archive: PathBuf,
        archive_compiler_executable: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum DistAuth {
    #[serde(rename = "token")]
    Token { token: String },
    #[serde(rename = "oauth2_code_grant_pkce")]
    Oauth2CodeGrantPKCE {
        client_id: String,
        auth_url: String,
        token_url: String,
    },
    #[serde(rename = "oauth2_implicit")]
    Oauth2Implicit { client_id: String, auth_url: String },
}

impl DistAuth {
    fn with_env_or_config(&self) -> Self {
        let mut env_or_cfg_token = env::var("SCCACHE_DIST_AUTH_TOKEN");
        let mut env_or_cfg_client_id = env::var("SCCACHE_DIST_AUTH_CLIENT_ID");
        let mut env_or_cfg_auth_url = env::var("SCCACHE_DIST_AUTH_URL");
        let mut env_or_cfg_token_url = env::var("SCCACHE_DIST_AUTH_TOKEN_URL");

        match self {
            Self::Token { token } => {
                env_or_cfg_token = env_or_cfg_token.or(Ok(token.clone()));
            }
            Self::Oauth2CodeGrantPKCE {
                client_id,
                auth_url,
                token_url,
            } => {
                env_or_cfg_client_id = env_or_cfg_client_id.or(Ok(client_id.clone()));
                env_or_cfg_auth_url = env_or_cfg_auth_url.or(Ok(auth_url.clone()));
                env_or_cfg_token_url = env_or_cfg_token_url.or(Ok(token_url.clone()));
            }
            Self::Oauth2Implicit {
                client_id,
                auth_url,
            } => {
                env_or_cfg_client_id = env_or_cfg_client_id.or(Ok(client_id.clone()));
                env_or_cfg_auth_url = env_or_cfg_auth_url.or(Ok(auth_url.clone()));
            }
        }

        let into_token = || {
            env_or_cfg_token
                .map(|token| Self::Token { token })
                .unwrap_or(self.clone())
        };

        let into_oauth2_code_grant_pkce = || {
            env_or_cfg_client_id
                .iter()
                .zip(env_or_cfg_auth_url.iter())
                .zip(env_or_cfg_token_url.iter())
                .map(
                    |((client_id, auth_url), token_url)| Self::Oauth2CodeGrantPKCE {
                        client_id: client_id.clone(),
                        auth_url: auth_url.clone(),
                        token_url: token_url.clone(),
                    },
                )
                .next_back()
                .unwrap_or(self.clone())
        };

        let into_oauth2_implicit = || {
            env_or_cfg_client_id
                .iter()
                .zip(env_or_cfg_auth_url.iter())
                .map(|(client_id, auth_url)| Self::Oauth2Implicit {
                    client_id: client_id.clone(),
                    auth_url: auth_url.clone(),
                })
                .next_back()
                .unwrap_or(self.clone())
        };

        if let Ok(auth_type) = env::var("SCCACHE_DIST_AUTH_TYPE") {
            match auth_type.as_ref() {
                "token" => into_token(),
                "oauth2_code_grant_pkce" => into_oauth2_code_grant_pkce(),
                "oauth2_implicit" => into_oauth2_implicit(),
                _ => {
                    warn!("Unknown SCCACHE_DIST_AUTH_TYPE {auth_type:?}");
                    self.clone()
                }
            }
        } else {
            match self {
                Self::Token { .. } => into_token(),
                Self::Oauth2CodeGrantPKCE { .. } => into_oauth2_code_grant_pkce(),
                Self::Oauth2Implicit { .. } => into_oauth2_implicit(),
            }
        }
    }
}

// Convert a type = "mozilla" immediately into an actual oauth configuration
// https://github.com/serde-rs/serde/issues/595 could help if implemented
impl<'a> Deserialize<'a> for DistAuth {
    fn deserialize<D>(deserializer: D) -> StdResult<Self, D::Error>
    where
        D: Deserializer<'a>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        #[serde(tag = "type")]
        pub enum Helper {
            #[serde(rename = "token")]
            Token { token: String },
            #[serde(rename = "oauth2_code_grant_pkce")]
            Oauth2CodeGrantPKCE {
                client_id: String,
                auth_url: String,
                token_url: String,
            },
            #[serde(rename = "oauth2_implicit")]
            Oauth2Implicit { client_id: String, auth_url: String },
        }

        let helper: Helper = Deserialize::deserialize(deserializer)?;

        Ok(match helper {
            Helper::Token { token } => DistAuth::Token { token },
            Helper::Oauth2CodeGrantPKCE {
                client_id,
                auth_url,
                token_url,
            } => DistAuth::Oauth2CodeGrantPKCE {
                client_id,
                auth_url,
                token_url,
            },
            Helper::Oauth2Implicit {
                client_id,
                auth_url,
            } => DistAuth::Oauth2Implicit {
                client_id,
                auth_url,
            },
        })
    }
}

impl Default for DistAuth {
    fn default() -> Self {
        DistAuth::Token {
            token: INSECURE_DIST_CLIENT_TOKEN.to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct DistNetworking {
    pub connect_timeout: u32,
    pub request_timeout: u32,
    pub connection_pool: bool,
    pub max_connections: u32,
    pub keepalive: DistNetworkingKeepalive,
}

impl DistNetworking {
    pub fn with_env_or_config(self) -> Self {
        Self {
            connect_timeout: number_from_env_var("SCCACHE_DIST_CONNECT_TIMEOUT")
                .map(|val| val.unwrap_or(self.connect_timeout))
                .unwrap_or(self.connect_timeout),
            request_timeout: number_from_env_var("SCCACHE_DIST_REQUEST_TIMEOUT")
                .map(|val| val.unwrap_or(self.request_timeout))
                .unwrap_or(self.request_timeout),
            connection_pool: bool_from_env_var("SCCACHE_DIST_CONNECTION_POOL")
                .map(|val| val.unwrap_or(self.connection_pool))
                .unwrap_or(self.connection_pool),
            max_connections: number_from_env_var("SCCACHE_DIST_MAX_CONNECTIONS")
                .map(|val| val.unwrap_or(self.max_connections))
                .unwrap_or(self.max_connections),
            keepalive: self.keepalive.with_env_or_config(),
        }
    }
}

impl Default for DistNetworking {
    fn default() -> Self {
        Self {
            // Default timeout for connections to an sccache-dist server
            connect_timeout: 30,
            // Default timeout for compile requests to an sccache-dist server.
            // Users should set their load balancer's idle timeout to match or
            // exceed this value.
            request_timeout: 600,
            // Default to using reqwest's HTTP/2 connection pool
            connection_pool: true,
            // Default to using 8 separate reqwest clients to maximize HTTP/2 throughput
            max_connections: 8,
            keepalive: Default::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct DistNetworkingKeepalive {
    pub enabled: bool,
    pub timeout: u64,
    pub interval: u64,
}

impl DistNetworkingKeepalive {
    pub fn with_env_or_config(self) -> Self {
        Self {
            enabled: bool_from_env_var("SCCACHE_DIST_KEEPALIVE_ENABLED")
                .map(|val| val.unwrap_or(self.enabled))
                .unwrap_or(self.enabled),
            interval: number_from_env_var("SCCACHE_DIST_KEEPALIVE_INTERVAL")
                .map(|val| val.unwrap_or(self.interval))
                .unwrap_or(self.interval),
            timeout: number_from_env_var("SCCACHE_DIST_KEEPALIVE_TIMEOUT")
                .map(|val| val.unwrap_or(self.timeout))
                .unwrap_or(self.timeout),
        }
    }
}

impl Default for DistNetworkingKeepalive {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: 20,
            timeout: 600,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct DistConfig {
    pub auth: DistAuth,
    pub cache_dir: PathBuf,
    pub fallback_to_local_compile: bool,
    // f64 to allow `max_retries = inf`, i.e. never fallback to local compile
    pub max_retries: f64,
    pub net: DistNetworking,
    pub rewrite_includes_only: bool,
    #[cfg(any(feature = "dist-client", feature = "dist-server"))]
    pub scheduler_url: Option<HTTPUrl>,
    #[cfg(not(any(feature = "dist-client", feature = "dist-server")))]
    pub scheduler_url: Option<String>,
    pub toolchains: Vec<DistToolchainConfig>,
    #[serde(deserialize_with = "deserialize_size_from_str")]
    pub toolchain_cache_size: u64,
}

impl DistConfig {
    pub fn with_env_or_config(self) -> Self {
        Self {
            auth: self.auth.with_env_or_config(),
            net: self.net.with_env_or_config(),
            #[cfg(any(feature = "dist-client", feature = "dist-server"))]
            scheduler_url: env::var("SCCACHE_DIST_SCHEDULER_URL")
                .map_err(Into::into)
                .and_then(parse_http_url)
                .map(HTTPUrl::from_url)
                .ok()
                .or(self.scheduler_url),
            fallback_to_local_compile: bool_from_env_var("SCCACHE_DIST_FALLBACK_TO_LOCAL_COMPILE")
                .map(|val| val.unwrap_or(self.fallback_to_local_compile))
                .unwrap_or(self.fallback_to_local_compile),
            max_retries: number_from_env_var("SCCACHE_DIST_MAX_RETRIES")
                .map(|val| val.unwrap_or(self.max_retries))
                .unwrap_or(self.max_retries),
            rewrite_includes_only: bool_from_env_var("SCCACHE_DIST_REWRITE_INCLUDES_ONLY")
                .map(|val| val.unwrap_or(self.rewrite_includes_only))
                .unwrap_or(self.rewrite_includes_only),
            ..self
        }
    }
}

impl Default for DistConfig {
    fn default() -> Self {
        Self {
            auth: Default::default(),
            cache_dir: default_dist_cache_dir(),
            fallback_to_local_compile: true,
            max_retries: 0f64,
            net: Default::default(),
            rewrite_includes_only: false,
            scheduler_url: Default::default(),
            toolchains: Default::default(),
            toolchain_cache_size: default_toolchain_cache_size(),
        }
    }
}

// TODO: fields only pub for tests
#[derive(Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub cache: CacheConfigs,
    pub dist: DistConfig,
    pub server_startup_timeout_ms: Option<u64>,
    /// Base directories to strip from paths for cache key computation.
    pub basedirs: Vec<String>,
    pub client_side_mode: bool,
}

// If the file doesn't exist or we can't read it, log the issue and proceed. If the
// config exists but doesn't parse then something is wrong - return an error.
pub fn try_read_config_file<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    debug!("Attempting to read config file at {path:?}");
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            debug!("Couldn't open config file: {e}");
            return Ok(None);
        }
    };

    let mut string = String::new();
    match file.read_to_string(&mut string) {
        Ok(_) => (),
        Err(e) => {
            warn!("Failed to read config file: {e}");
            return Ok(None);
        }
    }

    let res = if path.extension().is_some_and(|e| e == "json") {
        serde_json::from_str(&string)
            .with_context(|| format!("Failed to load json config file from {}", path.display()))?
    } else {
        toml::from_str(&string)
            .with_context(|| format!("Failed to load toml config file from {}", path.display()))?
    };

    Ok(Some(res))
}

#[derive(Debug)]
pub struct EnvConfig {
    cache: CacheConfigs,
    basedirs: Option<Vec<String>>,
    client_side_mode: Option<bool>,
}

fn string_from_env_var(env_var_name: &str) -> Option<String> {
    env::var(env_var_name).ok().filter(|s| !s.is_empty())
}

fn key_prefix_from_env_var(env_var_name: &str) -> Option<String> {
    string_from_env_var(env_var_name)
        .as_ref()
        .map(|s| s.trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
}

fn cache_mode_from_env_var(env_var_name: &str) -> Option<CacheModeConfig> {
    string_from_env_var(env_var_name).and_then(|value| match value.to_uppercase().as_str() {
        "READ_ONLY" => Some(CacheModeConfig::ReadOnly),
        "READ_WRITE" => Some(CacheModeConfig::ReadWrite),
        _ => {
            warn!("{env_var_name} must be 'READ_ONLY' or 'READ_WRITE'");
            None
        }
    })
}

pub fn number_from_env_var<A: std::str::FromStr>(env_var_name: &str) -> Option<Result<A>>
where
    <A as FromStr>::Err: fmt::Debug,
{
    let value = string_from_env_var(env_var_name)?;

    value
        .parse::<A>()
        .map_err(|err| anyhow!("{env_var_name} value is invalid: {err:?}"))
        .into()
}

pub fn bool_from_env_var(env_var_name: &str) -> Result<Option<bool>> {
    string_from_env_var(env_var_name)
        .map(|value| match value.to_lowercase().as_str() {
            "true" | "on" | "1" => Ok(true),
            "false" | "off" | "0" => Ok(false),
            _ => bail!("{env_var_name} must be 'true', 'on', '1', 'false', 'off' or '0'."),
        })
        .transpose()
}

fn config_from_env<'a>(envvar_prefix: impl Into<Option<&'a str>>) -> Result<EnvConfig> {
    let envvar_prefix = envvar_prefix.into().unwrap_or("SCCACHE_").to_owned();
    let envvar = |key: &str| envvar_prefix.clone() + key;

    // ======= AWS =======
    let s3 = if let Some(bucket) = string_from_env_var(&envvar("BUCKET")) {
        let region = string_from_env_var(&envvar("REGION"));
        let no_credentials = bool_from_env_var(&envvar("S3_NO_CREDENTIALS"))?.unwrap_or(false);
        let use_ssl = bool_from_env_var(&envvar("S3_USE_SSL"))?;
        let server_side_encryption = bool_from_env_var(&envvar("S3_SERVER_SIDE_ENCRYPTION"))?;
        let endpoint = string_from_env_var(&envvar("ENDPOINT"));
        let key_prefix = key_prefix_from_env_var(&envvar("S3_KEY_PREFIX")).unwrap_or_default();
        let enable_virtual_host_style = bool_from_env_var(&envvar("S3_ENABLE_VIRTUAL_HOST_STYLE"))?;
        let rw_mode =
            cache_mode_from_env_var(&envvar("S3_RW_MODE")).unwrap_or(CacheModeConfig::ReadWrite);

        Some(S3CacheConfig {
            bucket,
            region,
            no_credentials,
            key_prefix,
            endpoint,
            use_ssl,
            server_side_encryption,
            enable_virtual_host_style,
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "S3_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("S3_PREPROCESSOR_CACHE_KEY_PREFIX"))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("S3_CACHE_ORDER"))
                .unwrap_or(Ok(S3CacheConfig::default_order()))
                .unwrap_or(S3CacheConfig::default_order()),
            rw_mode,
        })
    } else {
        None
    };

    if s3.as_ref().map(|s3| s3.no_credentials).unwrap_or_default()
        && (env::var_os("AWS_ACCESS_KEY_ID").is_some()
            || env::var_os("AWS_SECRET_ACCESS_KEY").is_some())
    {
        bail!(
            "If setting S3 credentials, {} must not be set.",
            envvar("S3_NO_CREDENTIALS")
        );
    }

    // ======= redis =======
    let redis = match (
        string_from_env_var(&envvar("REDIS")),
        string_from_env_var(&envvar("REDIS_ENDPOINT")),
        string_from_env_var(&envvar("REDIS_CLUSTER_ENDPOINTS")),
        string_from_env_var(&envvar("REDIS_READER_ENDPOINTS")),
    ) {
        (None, None, None, None) => None,
        (url, endpoint, cluster_endpoints, reader_endpoints) => {
            let db = number_from_env_var(&envvar("REDIS_DB"))
                .transpose()?
                .unwrap_or(DEFAULT_REDIS_DB);

            let username = string_from_env_var(&envvar("REDIS_USERNAME"));
            let password = string_from_env_var(&envvar("REDIS_PASSWORD"));

            let ttl = number_from_env_var(&envvar("REDIS_EXPIRATION"))
                .or_else(|| number_from_env_var(&envvar("REDIS_TTL")))
                .transpose()?
                .unwrap_or(DEFAULT_REDIS_CACHE_TTL);

            let connection_pool_max_size =
                number_from_env_var(&envvar("REDIS_CONNECTION_POOL_SIZE"))
                    .transpose()?
                    .unwrap_or(10);

            let key_prefix =
                key_prefix_from_env_var(&envvar("REDIS_KEY_PREFIX")).unwrap_or_default();

            let rw_mode = cache_mode_from_env_var(&envvar("REDIS_RW_MODE"))
                .unwrap_or(CacheModeConfig::ReadWrite);

            Some(RedisCacheConfig {
                url,
                endpoint,
                cluster_endpoints,
                reader_endpoints,
                username,
                password,
                db,
                ttl,
                key_prefix,
                connection_pool_max_size: Some(connection_pool_max_size),
                preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                    use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                        "REDIS_USE_PREPROCESSOR_CACHE_MODE",
                    ))?
                    .unwrap_or_default(),
                    key_prefix: key_prefix_from_env_var(&envvar(
                        "REDIS_PREPROCESSOR_CACHE_KEY_PREFIX",
                    ))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
                }),
                order: number_from_env_var(&envvar("REDIS_CACHE_ORDER"))
                    .unwrap_or(Ok(RedisCacheConfig::default_order()))
                    .unwrap_or(RedisCacheConfig::default_order()),
                rw_mode,
            })
        }
    };

    if env::var_os(envvar("REDIS_EXPIRATION")).is_some()
        && env::var_os(envvar("REDIS_TTL")).is_some()
    {
        bail!(
            "You mustn't set both {} and {}. Use only one.",
            envvar("REDIS_EXPIRATION"),
            envvar("REDIS_TTL")
        );
    }

    // ======= memcached =======
    let memcached = if let Some(url) = string_from_env_var(&envvar("MEMCACHED"))
        .or_else(|| string_from_env_var(&envvar("MEMCACHED_ENDPOINT")))
    {
        let username = string_from_env_var(&envvar("MEMCACHED_USERNAME"));
        let password = string_from_env_var(&envvar("MEMCACHED_PASSWORD"));

        let expiration = number_from_env_var(&envvar("MEMCACHED_EXPIRATION"))
            .transpose()?
            .unwrap_or(DEFAULT_MEMCACHED_CACHE_EXPIRATION);

        let connection_pool_max_size =
            number_from_env_var(&envvar("MEMCACHED_CONNECTION_POOL_SIZE"))
                .transpose()?
                .unwrap_or(10);

        let key_prefix =
            key_prefix_from_env_var(&envvar("MEMCACHED_KEY_PREFIX")).unwrap_or_default();

        let rw_mode = cache_mode_from_env_var(&envvar("MEMCACHED_RW_MODE"))
            .unwrap_or(CacheModeConfig::ReadWrite);

        Some(MemcachedCacheConfig {
            url,
            username,
            password,
            expiration,
            key_prefix,
            connection_pool_max_size: Some(connection_pool_max_size),
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "MEMCACHED_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar(
                    "MEMCACHED_PREPROCESSOR_CACHE_KEY_PREFIX",
                ))
                .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("MEMCACHED_CACHE_ORDER"))
                .unwrap_or(Ok(MemcachedCacheConfig::default_order()))
                .unwrap_or(MemcachedCacheConfig::default_order()),
            rw_mode,
        })
    } else {
        None
    };

    if env::var_os(envvar("MEMCACHED")).is_some()
        && env::var_os(envvar("MEMCACHED_ENDPOINT")).is_some()
    {
        bail!(
            "You mustn't set both {} and {}. Please, use only {}.",
            envvar("MEMCACHED"),
            envvar("MEMCACHED_ENDPOINT"),
            envvar("MEMCACHED_ENDPOINT")
        );
    }

    // ======= GCP/GCS =======
    if (string_from_env_var(&envvar("GCS_CREDENTIALS_URL")).is_some()
        || string_from_env_var(&envvar("GCS_OAUTH_URL")).is_some()
        || string_from_env_var(&envvar("GCS_KEY_PATH")).is_some())
        && string_from_env_var(&envvar("GCS_BUCKET")).is_none()
    {
        bail!(
            "If setting GCS credentials, {} and an auth mechanism need to be set.",
            envvar("GCS_BUCKET")
        );
    }

    let gcs = string_from_env_var(&envvar("GCS_BUCKET")).map(|bucket| {
        let key_prefix = key_prefix_from_env_var(&envvar("GCS_KEY_PREFIX")).unwrap_or_default();

        if string_from_env_var(&envvar("GCS_OAUTH_URL")).is_some() {
            eprintln!("{} has been deprecated", envvar("GCS_OAUTH_URL"));
            eprintln!("if you intend to use vm metadata for auth, please set correct service account instead");
        }

        let credential_url = string_from_env_var(&envvar("GCS_CREDENTIALS_URL"));

        let cred_path = string_from_env_var(&envvar("GCS_KEY_PATH"));
        let service_account = string_from_env_var(&envvar("GCS_SERVICE_ACCOUNT"));

        let rw_mode = cache_mode_from_env_var(&envvar("GCS_RW_MODE")).unwrap_or_else(|| {
            warn!("No valid SCCACHE_GCS_RW_MODE value was found -- defaulting to READ_ONLY.");
            CacheModeConfig::ReadOnly
        });

        GCSCacheConfig {
            bucket,
            key_prefix,
            cred_path,
            service_account,
            rw_mode,
            credential_url,
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "GCS_USE_PREPROCESSOR_CACHE_MODE",
                ))
                .unwrap_or(None)
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("GCS_PREPROCESSOR_CACHE_KEY_PREFIX")) //
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("GCS_CACHE_ORDER"))
                .unwrap_or(Ok(GCSCacheConfig::default_order()))
                .unwrap_or(GCSCacheConfig::default_order()),
        }
    });

    // ======= GHA =======
    let gha_rw_mode: CacheModeConfig =
        cache_mode_from_env_var(&envvar("GHA_RW_MODE")).unwrap_or(CacheModeConfig::ReadWrite);
    let gha = if let Some(version) = string_from_env_var(&envvar("GHA_VERSION")) {
        // If SCCACHE_GHA_VERSION has been set, we don't need to check
        // SCCACHE_GHA_ENABLED's value anymore.
        Some(GHACacheConfig {
            enabled: true,
            version,
            key_prefix: key_prefix_from_env_var(&envvar("GHA_CACHE_KEY_PREFIX"))
                .unwrap_or_default(),
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "GHA_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("GHA_PREPROCESSOR_CACHE_KEY_PREFIX"))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("GHA_CACHE_ORDER"))
                .unwrap_or(Ok(GHACacheConfig::default_order()))
                .unwrap_or(GHACacheConfig::default_order()),
            rw_mode: gha_rw_mode,
        })
    } else if bool_from_env_var(&envvar("GHA_ENABLED"))?.unwrap_or(false) {
        // If only SCCACHE_GHA_ENABLED has been set to the true value, enable with
        // default version.
        Some(GHACacheConfig {
            enabled: true,
            version: String::new(),
            key_prefix: key_prefix_from_env_var(&envvar("GHA_CACHE_KEY_PREFIX"))
                .unwrap_or_default(),
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "GHA_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("GHA_PREPROCESSOR_CACHE_KEY_PREFIX"))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("GHA_CACHE_ORDER"))
                .unwrap_or(Ok(GHACacheConfig::default_order()))
                .unwrap_or(GHACacheConfig::default_order()),
            rw_mode: gha_rw_mode,
        })
    } else {
        None
    };

    // ======= Azure =======
    let azure = if let (Some(connection_string), Some(container)) = (
        string_from_env_var(&envvar("AZURE_CONNECTION_STRING")),
        string_from_env_var(&envvar("AZURE_BLOB_CONTAINER")),
    ) {
        let key_prefix = key_prefix_from_env_var(&envvar("AZURE_KEY_PREFIX")).unwrap_or_default();
        let rw_mode =
            cache_mode_from_env_var(&envvar("AZURE_RW_MODE")).unwrap_or(CacheModeConfig::ReadWrite);

        Some(AzureCacheConfig {
            connection_string,
            container,
            key_prefix,
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "AZURE_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("AZURE_PREPROCESSOR_CACHE_KEY_PREFIX"))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("AZURE_CACHE_ORDER"))
                .unwrap_or(Ok(AzureCacheConfig::default_order()))
                .unwrap_or(AzureCacheConfig::default_order()),
            rw_mode,
        })
    } else {
        None
    };

    // ======= WebDAV =======
    let webdav = if let Some(endpoint) = string_from_env_var(&envvar("WEBDAV_ENDPOINT")) {
        let key_prefix = key_prefix_from_env_var(&envvar("WEBDAV_KEY_PREFIX")).unwrap_or_default();
        let username = string_from_env_var(&envvar("WEBDAV_USERNAME"));
        let password = string_from_env_var(&envvar("WEBDAV_PASSWORD"));
        let token = string_from_env_var(&envvar("WEBDAV_TOKEN"));
        let rw_mode = cache_mode_from_env_var(&envvar("WEBDAV_RW_MODE"))
            .unwrap_or(CacheModeConfig::ReadWrite);

        Some(WebdavCacheConfig {
            endpoint,
            key_prefix,
            username,
            password,
            token,
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "WEBDAV_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar(
                    "WEBDAV_PREPROCESSOR_CACHE_KEY_PREFIX",
                ))
                .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("WEBDAV_CACHE_ORDER"))
                .unwrap_or(Ok(WebdavCacheConfig::default_order()))
                .unwrap_or(WebdavCacheConfig::default_order()),
            rw_mode,
        })
    } else {
        None
    };

    // ======= OSS =======
    let oss = if let Some(bucket) = string_from_env_var(&envvar("OSS_BUCKET")) {
        let endpoint = string_from_env_var(&envvar("OSS_ENDPOINT"));
        let key_prefix = key_prefix_from_env_var(&envvar("OSS_KEY_PREFIX")).unwrap_or_default();

        let no_credentials = bool_from_env_var(&envvar("OSS_NO_CREDENTIALS"))?.unwrap_or(false);

        let rw_mode =
            cache_mode_from_env_var(&envvar("OSS_RW_MODE")).unwrap_or(CacheModeConfig::ReadWrite);

        Some(OSSCacheConfig {
            bucket,
            endpoint,
            key_prefix,
            no_credentials,
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "OSS_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("OSS_PREPROCESSOR_CACHE_KEY_PREFIX"))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("OSS_CACHE_ORDER"))
                .unwrap_or(Ok(OSSCacheConfig::default_order()))
                .unwrap_or(OSSCacheConfig::default_order()),
            rw_mode,
        })
    } else {
        None
    };

    if oss
        .as_ref()
        .map(|oss| oss.no_credentials)
        .unwrap_or_default()
        && (env::var_os("ALIBABA_CLOUD_ACCESS_KEY_ID").is_some()
            || env::var_os("ALIBABA_CLOUD_ACCESS_KEY_SECRET").is_some())
    {
        bail!(
            "If setting OSS credentials, {} must not be set.",
            envvar("OSS_NO_CREDENTIALS")
        );
    }

    // ======= COS =======
    let cos = if let Some(bucket) = string_from_env_var(&envvar("COS_BUCKET")) {
        let endpoint = string_from_env_var(&envvar("COS_ENDPOINT"));
        let key_prefix = key_prefix_from_env_var(&envvar("COS_KEY_PREFIX")).unwrap_or_default();

        let rw_mode =
            cache_mode_from_env_var(&envvar("COS_RW_MODE")).unwrap_or(CacheModeConfig::ReadWrite);

        Some(COSCacheConfig {
            bucket,
            endpoint,
            key_prefix,
            preprocessor_cache_mode: Some(PreprocessorCacheModeConfig {
                use_preprocessor_cache_mode: bool_from_env_var(&envvar(
                    "COS_USE_PREPROCESSOR_CACHE_MODE",
                ))?
                .unwrap_or_default(),
                key_prefix: key_prefix_from_env_var(&envvar("COS_PREPROCESSOR_CACHE_KEY_PREFIX"))
                    .unwrap_or_else(PreprocessorCacheModeConfig::default_key_prefix),
            }),
            order: number_from_env_var(&envvar("COS_CACHE_ORDER"))
                .unwrap_or(Ok(COSCacheConfig::default_order()))
                .unwrap_or(COSCacheConfig::default_order()),
            rw_mode,
        })
    } else {
        None
    };

    // ======= Local =======
    let disk_dir = env::var_os(envvar("DIR"))
        .or(env::var_os(envvar("DISK_CACHE_PATH")))
        .map(PathBuf::from);
    let disk_sz = string_from_env_var(&envvar("CACHE_SIZE"))
        .or(string_from_env_var(&envvar("DISK_CACHE_SIZE")))
        .and_then(|v| parse_size(&v));
    let disk_order = number_from_env_var(&envvar("DISK_CACHE_ORDER")).and_then(|o| o.ok());

    let mut preprocessor_cache_mode = PreprocessorCacheModeConfig::activated();
    let preprocessor_mode_overridden = if let Some(value) = bool_from_env_var(&envvar("DIRECT"))? {
        preprocessor_cache_mode.use_preprocessor_cache_mode = value;
        true
    } else if let Some(value) = bool_from_env_var(&envvar("DISK_USE_PREPROCESSOR_CACHE_MODE"))? {
        preprocessor_cache_mode.use_preprocessor_cache_mode = value;
        true
    } else {
        false
    };

    if preprocessor_cache_mode.use_preprocessor_cache_mode
        && env::var(envvar("DISK_PREPROCESSOR_CACHE_KEY_PREFIX")).is_ok()
    {
        preprocessor_cache_mode.key_prefix =
            key_prefix_from_env_var(&envvar("DISK_PREPROCESSOR_CACHE_KEY_PREFIX"))
                .unwrap_or(preprocessor_cache_mode.key_prefix);
    }

    let (disk_rw_mode, disk_rw_mode_overridden) =
        match cache_mode_from_env_var(&envvar("LOCAL_RW_MODE")) {
            Some(mode) => (mode, true),
            _ => (CacheModeConfig::ReadWrite, false),
        };

    let any_overridden = disk_dir.is_some()
        || disk_sz.is_some()
        || disk_order.is_some()
        || preprocessor_mode_overridden
        || disk_rw_mode_overridden;
    let disk = if any_overridden {
        Some(DiskCacheConfig {
            dir: disk_dir.unwrap_or_else(default_disk_cache_dir),
            size: disk_sz.unwrap_or_else(default_disk_cache_size),
            preprocessor_cache_mode,
            rw_mode: disk_rw_mode,
            order: disk_order.unwrap_or(DiskCacheConfig::default_order()),
        })
    } else {
        None
    };

    // Parse multi-level cache configuration
    let multilevel = if let Some(chain_str) = string_from_env_var(&envvar("MULTILEVEL_CHAIN")) {
        let chain: Vec<String> = chain_str
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .collect();

        let write_error_policy = string_from_env_var(&envvar("MULTILEVEL_WRITE_ERROR_POLICY"))
            .and_then(|s| s.parse::<WriteErrorPolicy>().ok())
            .unwrap_or_default();

        Some(MultiLevelConfig {
            chain,
            write_error_policy,
        })
    } else {
        None
    };

    let cache = CacheConfigs {
        azure,
        disk,
        gcs,
        gha,
        memcached,
        redis,
        s3,
        webdav,
        oss,
        cos,
        multilevel,
    };

    // ======= Base directory =======
    // Support multiple paths separated by ';' on Windows and ':' on other platforms
    // to match PATH behavior.
    #[cfg(target_os = "windows")]
    let split_symbol = ';';
    #[cfg(not(target_os = "windows"))]
    let split_symbol = ':';
    let basedirs = env::var_os("SCCACHE_BASEDIRS").map(|s| {
        s.to_string_lossy()
            .split(split_symbol)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect()
    });

    let client_side_mode = bool_from_env_var("SCCACHE_CLIENT_SIDE")?;

    Ok(EnvConfig {
        cache,
        basedirs,
        client_side_mode,
    })
}

// The directories crate changed the location of `config_dir` on macos in version 3,
// so we also check the config in `preference_dir` (new in that version), which
// corresponds to the old location, for compatibility with older setups.
fn config_file(env_var: &str, leaf: &str) -> PathBuf {
    if let Some(env_value) = env::var_os(env_var) {
        return env_value.into();
    }
    let dirs =
        ProjectDirs::from("", ORGANIZATION, APP_NAME).expect("Unable to get config directory");
    // If the new location exists, use that.
    let path = dirs.config_dir().join(leaf);
    if path.exists() {
        return path;
    }
    // If the old location exists, use that.
    let path = dirs.preference_dir().join(leaf);
    if path.exists() {
        return path;
    }
    // Otherwise, use the new location.
    dirs.config_dir().join(leaf)
}

#[derive(Debug, Default, PartialEq)]
pub struct Config {
    pub caches: CacheConfigs,
    pub dist: DistConfig,
    pub server_startup_timeout: Option<Duration>,
    /// Base directory (or directories) to strip from paths for cache key computation.
    /// Similar to ccache's CCACHE_BASEDIR.
    pub basedirs: Vec<Vec<u8>>,
    pub client_side_mode: bool,
}

impl Config {
    pub fn load() -> Result<Self> {
        let env_conf = config_from_env("SCCACHE_")?;

        let file_conf_path = config_file("SCCACHE_CONF", "config");
        let file_conf = try_read_config_file(&file_conf_path)
            .context("Failed to load config file")?
            .unwrap_or_default();

        Self::from_env_and_file_configs(env_conf, file_conf)
    }

    fn from_env_and_file_configs(env_conf: EnvConfig, file_conf: FileConfig) -> Result<Self> {
        let FileConfig {
            dist,
            server_startup_timeout_ms,
            ..
        } = file_conf;

        let server_startup_timeout = server_startup_timeout_ms.map(Duration::from_millis);

        let caches = CacheConfigs::default()
            .merge(file_conf.cache)
            .merge(env_conf.cache);

        // Environment variable takes precedence over file config if it is set
        let basedirs_raw = env_conf.basedirs.unwrap_or(file_conf.basedirs);

        // Validate that all basedirs are absolute paths
        // basedirs_raw is Vec<PathBuf>
        let mut basedirs = Vec::with_capacity(basedirs_raw.len());
        for d in basedirs_raw {
            let p = Utf8TypedPathBuf::from(d);
            if !p.is_absolute() {
                bail!("Basedir path must be absolute: {p:?}");
            }
            // Normalize basedir:
            // remove double separators, cur_dirs, parent_dirs, trailing slashes
            let p_norm = p.normalize();
            let mut bytes = p_norm.to_string().into_bytes();

            // Always add a trailing `/` to basedirs to ensure we only match complete path
            // components
            bytes.push(b'/');

            // normalize windows paths: use slashes and lowercase
            let normalized = {
                #[cfg(target_os = "windows")]
                {
                    normalize_win_path(&bytes)
                }

                #[cfg(not(target_os = "windows"))]
                {
                    bytes
                }
            };
            // push only if not already present
            if !basedirs.contains(&normalized) {
                basedirs.push(normalized);
            }
        }

        if !basedirs.is_empty() && log::log_enabled!(log::Level::Debug) {
            let basedirs_str: Vec<String> = basedirs
                .iter()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .collect();
            debug!("Using basedirs for path normalization: {basedirs_str:?}");
        }

        let client_side_mode = env_conf.client_side_mode.unwrap_or(file_conf.client_side_mode)
            // Logging always writes to stderr in the client process, disregarding
            // SCCACHE_ERROR_LOG; Even if SCCACHE_ERROR_LOG was handled, there would
            // be write races from multiple processes. Until those are addressed, we
            // can't leave the client mode enabled alongside logging.
            && std::env::var_os(crate::LOGGING_ENV).is_none()
            // Client-side mode is also not compatible with distributed compilation.
            && dist.scheduler_url.is_none();

        Ok(Self {
            caches,
            dist: dist.with_env_or_config(),
            server_startup_timeout,
            basedirs,
            client_side_mode,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct CachedDistConfig {
    pub auth_tokens: HashMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
#[serde(deny_unknown_fields)]
pub struct CachedFileConfig {
    pub dist: CachedDistConfig,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CachedConfig(());

impl CachedConfig {
    pub fn load() -> Result<Self> {
        let mut cached_file_config = CACHED_CONFIG.lock().unwrap();

        if cached_file_config.is_none() {
            let cfg = Self::load_file_config().context("Unable to initialise cached config")?;
            *cached_file_config = Some(cfg);
        }
        Ok(CachedConfig(()))
    }
    pub fn reload() -> Result<Self> {
        {
            let mut cached_file_config = CACHED_CONFIG.lock().unwrap();
            *cached_file_config = None;
        };
        Self::load()
    }
    pub fn with<F: FnOnce(&CachedFileConfig) -> T, T>(&self, f: F) -> T {
        let cached_file_config = CACHED_CONFIG.lock().unwrap();
        let cached_file_config = cached_file_config.as_ref().unwrap();

        f(cached_file_config)
    }
    pub fn with_mut<F: FnOnce(&mut CachedFileConfig)>(&self, f: F) -> Result<()> {
        let mut cached_file_config = CACHED_CONFIG.lock().unwrap();
        let cached_file_config = cached_file_config.as_mut().unwrap();

        let mut new_config = cached_file_config.clone();
        f(&mut new_config);
        Self::save_file_config(&new_config)?;
        *cached_file_config = new_config;
        Ok(())
    }

    fn file_config_path() -> PathBuf {
        config_file("SCCACHE_CACHED_CONF", "cached-config")
    }
    fn load_file_config() -> Result<CachedFileConfig> {
        let file_conf_path = &*CACHED_CONFIG_PATH;

        if !file_conf_path.exists() {
            let file_conf_dir = file_conf_path
                .parent()
                .expect("Cached conf file has no parent directory");
            if !file_conf_dir.is_dir() {
                fs::create_dir_all(file_conf_dir)
                    .context("Failed to create dir to hold cached config")?;
            }
            Self::save_file_config(&Default::default()).with_context(|| {
                format!(
                    "Unable to create cached config file at {}",
                    file_conf_path.display()
                )
            })?;
        }
        try_read_config_file(file_conf_path)
            .context("Failed to load cached config file")?
            .with_context(|| format!("Failed to load from {}", file_conf_path.display()))
    }
    fn save_file_config(c: &CachedFileConfig) -> Result<()> {
        let file_conf_path = &*CACHED_CONFIG_PATH;
        let mut file = File::create(file_conf_path).context("Could not open config for writing")?;
        file.write_all(toml::to_string(c).unwrap().as_bytes())
            .map_err(Into::into)
    }
}

#[cfg(feature = "dist-server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageBroker {
    #[serde(rename = "amqp")]
    AMQP(String),
    #[serde(rename = "redis")]
    Redis(String),
}

#[cfg(feature = "dist-server")]
impl MessageBroker {
    fn from_env() -> Option<MessageBroker> {
        None.or(std::env::var("AMQP_ADDR").ok().map(MessageBroker::AMQP))
            .or(std::env::var("REDIS_ADDR").ok().map(MessageBroker::Redis))
    }
}

#[cfg(feature = "dist-server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DogStatsDAggregationMode {
    #[serde(rename = "aggressive")]
    Aggressive,
    #[serde(rename = "conservative")]
    Conservative,
}

#[cfg(feature = "dist-server")]
impl DogStatsDAggregationMode {
    pub fn with_env_or_config(self) -> Self {
        match env::var("SCCACHE_DIST_DOGSTATSD_AGGREGATION_MODE").as_deref() {
            Ok("aggressive") => Self::Aggressive,
            Ok("conservative") => Self::Conservative,
            _ => self,
        }
    }
}

#[cfg(feature = "dist-server")]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DogStatsDMetricsConfig {
    pub remote_addr: String,
    // Write timeout (in milliseconds) for forwarding metrics to dogstatsd
    pub write_timeout: Option<u64>,
    // Maximum payload length for forwarding metrics
    pub maximum_payload_length: Option<usize>,
    // The aggregation mode for the exporter
    pub aggregation_mode: Option<DogStatsDAggregationMode>,
    // The flush interval of the aggregator
    pub flush_interval: Option<u64>,
    // Whether or not to enable telemetry for the exporter
    pub telemetry: Option<bool>,
    // Whether or not to enable histogram sampling
    pub histogram_sampling: Option<bool>,
    // The reservoir size for histogram sampling
    pub histogram_reservoir_size: Option<usize>,
    // Whether or not to send histograms as distributions
    pub histograms_as_distributions: Option<bool>,
}

#[cfg(feature = "dist-server")]
impl DogStatsDMetricsConfig {
    fn from_env() -> Option<Self> {
        env::var("SCCACHE_DIST_DOGSTATSD_ADDR")
            .ok()
            .map(|remote_addr| {
                Self {
                    remote_addr,
                    ..Default::default()
                }
                .with_env_or_config()
            })
    }

    pub fn with_env_or_config(self) -> Self {
        Self {
            remote_addr: env::var("SCCACHE_DIST_DOGSTATSD_ADDR")
                .ok()
                .unwrap_or(self.remote_addr),
            write_timeout: number_from_env_var("SCCACHE_DIST_DOGSTATSD_WRITE_TIMEOUT")
                .and_then(|val| val.ok().or(self.write_timeout)),
            maximum_payload_length: number_from_env_var(
                "SCCACHE_DIST_DOGSTATSD_MAXIMUM_PAYLOAD_LENGTH",
            )
            .and_then(|val| val.ok().or(self.maximum_payload_length)),
            aggregation_mode: self
                .aggregation_mode
                .map(|aggregation_mode| aggregation_mode.with_env_or_config()),
            flush_interval: number_from_env_var("SCCACHE_DIST_DOGSTATSD_FLUSH_INTERVAL")
                .and_then(|val| val.ok().or(self.flush_interval)),
            telemetry: bool_from_env_var("SCCACHE_DIST_DOGSTATSD_TELEMETRY")
                .transpose()
                .and_then(|val| val.ok().or(self.telemetry)),
            histogram_sampling: bool_from_env_var("SCCACHE_DIST_DOGSTATSD_HISTOGRAM_SAMPLING")
                .transpose()
                .and_then(|val| val.ok().or(self.histogram_sampling)),
            histogram_reservoir_size: number_from_env_var(
                "SCCACHE_DIST_DOGSTATSD_HISTOGRAM_RESERVOIR_SIZE",
            )
            .and_then(|val| val.ok().or(self.histogram_reservoir_size)),
            histograms_as_distributions: bool_from_env_var(
                "SCCACHE_DIST_DOGSTATSD_HISTOGRAMS_AS_DISTRIBUTIONS",
            )
            .transpose()
            .and_then(|val| val.ok().or(self.histograms_as_distributions)),
        }
    }
}

#[cfg(feature = "dist-server")]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
#[serde(deny_unknown_fields)]
pub enum PrometheusMetricsConfig {
    #[serde(rename = "bind")]
    ListenAddr {
        addr: Option<std::net::SocketAddr>,
        idle_timeout_secs: Option<u64>,
    },
    #[serde(rename = "path")]
    ListenPath {
        path: Option<String>,
        idle_timeout_secs: Option<u64>,
    },
    #[serde(rename = "push")]
    PushGateway {
        endpoint: String,
        // Interval (in milliseconds) to push metrics to prometheus
        interval: Option<u64>,
        username: Option<String>,
        password: Option<String>,
        http_method: Option<String>,
        idle_timeout_secs: Option<u64>,
    },
}

#[cfg(feature = "dist-server")]
impl PrometheusMetricsConfig {
    fn from_env() -> Option<Self> {
        match env::var("SCCACHE_DIST_PROMETHEUS_TYPE").as_deref() {
            Ok("bind") => Some(Self::ListenAddr {
                addr: env::var("SCCACHE_DIST_PROMETHEUS_LISTEN_ADDR")
                    .ok()
                    .and_then(|addr| std::net::SocketAddr::from_str(&addr).ok()),
                idle_timeout_secs: number_from_env_var("SCCACHE_DIST_PROMETHEUS_IDLE_TIMEOUT_SECS")
                    .transpose()
                    .unwrap_or(None),
            }),
            Ok("path") => Some(Self::ListenPath {
                path: env::var("SCCACHE_DIST_PROMETHEUS_LISTEN_PATH").ok(),
                idle_timeout_secs: number_from_env_var("SCCACHE_DIST_PROMETHEUS_IDLE_TIMEOUT_SECS")
                    .transpose()
                    .unwrap_or(None),
            }),
            Ok("push") => env::var("SCCACHE_DIST_PROMETHEUS_PUSH_ENDPOINT")
                .ok()
                .map(|endpoint| Self::PushGateway {
                    endpoint,
                    interval: number_from_env_var("SCCACHE_DIST_PROMETHEUS_PUSH_INTERVAL")
                        .transpose()
                        .unwrap_or(Some(10_000)),
                    username: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_USERNAME").ok(),
                    password: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_PASSWORD").ok(),
                    http_method: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_HTTP_METHOD").ok(),
                    idle_timeout_secs: number_from_env_var(
                        "SCCACHE_DIST_PROMETHEUS_IDLE_TIMEOUT_SECS",
                    )
                    .transpose()
                    .unwrap_or(None),
                }),
            _ => None,
        }
    }

    pub fn with_env_or_config(self) -> Self {
        match self {
            Self::ListenAddr {
                addr,
                idle_timeout_secs,
            } => Self::ListenAddr {
                addr: env::var("SCCACHE_DIST_PROMETHEUS_LISTEN_ADDR")
                    .ok()
                    .and_then(|addr| std::net::SocketAddr::from_str(&addr).ok())
                    .or(addr),
                idle_timeout_secs: number_from_env_var("SCCACHE_DIST_PROMETHEUS_IDLE_TIMEOUT_SECS")
                    .transpose()
                    .unwrap_or(idle_timeout_secs),
            },
            Self::ListenPath {
                path,
                idle_timeout_secs,
            } => Self::ListenPath {
                path: env::var("SCCACHE_DIST_PROMETHEUS_LISTEN_PATH")
                    .ok()
                    .or(path),
                idle_timeout_secs: number_from_env_var("SCCACHE_DIST_PROMETHEUS_IDLE_TIMEOUT_SECS")
                    .transpose()
                    .unwrap_or(idle_timeout_secs),
            },
            Self::PushGateway {
                endpoint,
                interval,
                username,
                password,
                http_method,
                idle_timeout_secs,
            } => Self::PushGateway {
                endpoint: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_ENDPOINT")
                    .ok()
                    .unwrap_or(endpoint),
                // Interval (in milliseconds) to push metrics to prometheus
                interval: number_from_env_var("SCCACHE_DIST_PROMETHEUS_PUSH_INTERVAL")
                    .transpose()
                    .unwrap_or(interval),
                username: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_USERNAME")
                    .ok()
                    .or(username),
                password: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_PASSWORD")
                    .ok()
                    .or(password),
                http_method: env::var("SCCACHE_DIST_PROMETHEUS_PUSH_HTTP_METHOD")
                    .ok()
                    .or(http_method),
                idle_timeout_secs: number_from_env_var("SCCACHE_DIST_PROMETHEUS_IDLE_TIMEOUT_SECS")
                    .transpose()
                    .unwrap_or(idle_timeout_secs),
            },
        }
    }
}

#[cfg(feature = "dist-server")]
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricsConfigs {
    pub dogstatsd: Option<DogStatsDMetricsConfig>,
    pub prometheus: Option<PrometheusMetricsConfig>,
}

#[cfg(feature = "dist-server")]
impl MetricsConfigs {
    pub fn with_env_or_config(self) -> Self {
        let mut metrics = match env::var("SCCACHE_DIST_METRICS_TYPE").as_deref() {
            Ok("dogstatsd") => Self {
                dogstatsd: self.dogstatsd.or_else(DogStatsDMetricsConfig::from_env),
                prometheus: None,
            },
            Ok("prometheus") => Self {
                dogstatsd: None,
                prometheus: self.prometheus.or_else(PrometheusMetricsConfig::from_env),
            },
            _ => self,
        };

        metrics.dogstatsd = metrics
            .dogstatsd
            .map(|dogstatsd| dogstatsd.with_env_or_config());

        metrics.prometheus = metrics
            .prometheus
            .map(|prometheus| prometheus.with_env_or_config());

        metrics
    }
}

#[cfg(feature = "dist-server")]
pub mod scheduler {
    use std::collections::HashMap;
    use std::env;
    use std::path::PathBuf;
    use std::{net::SocketAddr, str::FromStr};

    use crate::config::DistNetworkingKeepalive;
    use crate::errors::*;

    use itertools::Itertools;
    use serde::{Deserialize, Serialize};

    use super::{
        CacheConfigs, MessageBroker, MetricsConfigs, config_from_env, number_from_env_var,
        try_read_config_file,
    };

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum ClientAuth {
        #[default]
        #[serde(rename = "DANGEROUSLY_INSECURE")]
        Insecure,
        #[serde(rename = "token")]
        Token { token: String },
        #[serde(rename = "jwt_validate")]
        JwtValidate {
            #[serde(deserialize_with = "ClientAuth::string_or_list")]
            audience: Vec<String>,
            #[serde(deserialize_with = "ClientAuth::string_or_list")]
            issuer: Vec<String>,
            #[serde(deserialize_with = "ClientAuth::string_or_list")]
            jwks_url: Vec<String>,
            claims: Option<HashMap<String, String>>,
            leeway: Option<u64>,
        },
        #[serde(rename = "proxy_token")]
        ProxyToken {
            url: String,
            cache_secs: Option<u64>,
            decode: Option<ProxyTokenDecodeConfig>,
            rate_limit_on_error_count: Option<usize>,
            rate_limit_on_error_window_size_secs: Option<u64>,
        },
    }

    impl ClientAuth {
        pub fn with_env_or_config(self, idx: Option<usize>) -> Result<Self> {
            let suf = idx.map(|idx| format!("_{idx}")).unwrap_or_default();
            let sccache_dist_auth_token_env = format!("SCCACHE_DIST_AUTH_TOKEN{suf}");
            let sccache_dist_auth_audience_env = format!("SCCACHE_DIST_AUTH_AUDIENCE{suf}");
            let sccache_dist_auth_issuer_env = format!("SCCACHE_DIST_AUTH_ISSUER{suf}");
            let sccache_dist_auth_jwks_url_env = format!("SCCACHE_DIST_AUTH_JWKS_URL{suf}");
            let sccache_dist_auth_claims_env = format!("SCCACHE_DIST_AUTH_CLAIMS{suf}");
            let sccache_dist_auth_leeway_env = format!("SCCACHE_DIST_AUTH_LEEWAY{suf}");
            let sccache_dist_auth_url_env = format!("SCCACHE_DIST_AUTH_URL{suf}");
            let sccache_dist_auth_type_env = format!("SCCACHE_DIST_AUTH_TYPE{suf}");
            let sccache_dist_auth_cache_secs_env = format!("SCCACHE_DIST_AUTH_CACHE_SECS{suf}");
            let sccache_dist_auth_rate_limit_on_error_count_env =
                format!("SCCACHE_DIST_AUTH_RATE_LIMIT_ON_ERROR_COUNT{suf}");
            let sccache_dist_auth_rate_limit_on_error_window_size_secs_env =
                format!("SCCACHE_DIST_AUTH_RATE_LIMIT_ON_ERROR_WINDOW_SIZE_SECS{suf}");

            let mut env_or_cfg_token = env::var(&sccache_dist_auth_token_env);
            let mut env_or_cfg_audience =
                env::var(&sccache_dist_auth_audience_env).map(|audiences| {
                    audiences
                        .split(" ")
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                });
            let mut env_or_cfg_issuer = env::var(&sccache_dist_auth_issuer_env).map(|issuers| {
                issuers
                    .split(" ")
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            });
            let mut env_or_cfg_jwks_url =
                env::var(&sccache_dist_auth_jwks_url_env).map(|jwks_urls| {
                    jwks_urls
                        .split(" ")
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                });
            let mut env_or_cfg_claims = env::var(&sccache_dist_auth_claims_env)
                .map(|claims| {
                    claims
                        .split(" ")
                        .map(|pair| {
                            pair.split_once("=")
                                .map(|(key, val)| (key.to_string(), val.to_string()))
                                .unwrap_or_else(|| (pair.to_string(), "*".to_string()))
                        })
                        .collect::<_>()
                })
                .ok();
            let mut env_or_cfg_leeway =
                number_from_env_var(&sccache_dist_auth_leeway_env).and_then(|s| s.ok());

            let mut env_or_cfg_url = env::var(&sccache_dist_auth_url_env);
            let mut env_or_cfg_cache_secs =
                number_from_env_var(&sccache_dist_auth_cache_secs_env).and_then(|s| s.ok());
            let mut env_or_cfg_decode = ProxyTokenDecodeConfig::from_env(idx);
            let mut env_or_cfg_rate_limit_on_error_count =
                number_from_env_var(&sccache_dist_auth_rate_limit_on_error_count_env)
                    .and_then(|s| s.ok());
            let mut env_or_cfg_rate_limit_on_error_window_size_secs =
                number_from_env_var(&sccache_dist_auth_rate_limit_on_error_window_size_secs_env)
                    .and_then(|s| s.ok());

            match self.clone() {
                Self::Token { token } => {
                    env_or_cfg_token = env_or_cfg_token.or(Ok(token));
                }
                Self::JwtValidate {
                    audience,
                    issuer,
                    jwks_url,
                    claims,
                    leeway,
                } => {
                    env_or_cfg_audience = env_or_cfg_audience.or(Ok(audience));
                    env_or_cfg_issuer = env_or_cfg_issuer.or(Ok(issuer));
                    env_or_cfg_jwks_url = env_or_cfg_jwks_url.or(Ok(jwks_url));
                    env_or_cfg_claims = env_or_cfg_claims.or(claims);
                    env_or_cfg_leeway = env_or_cfg_leeway.or(leeway);
                }
                Self::ProxyToken {
                    url,
                    cache_secs,
                    decode,
                    rate_limit_on_error_count,
                    rate_limit_on_error_window_size_secs,
                } => {
                    env_or_cfg_url = env_or_cfg_url.or(Ok(url));
                    env_or_cfg_cache_secs = env_or_cfg_cache_secs.or(cache_secs);
                    env_or_cfg_decode =
                        env_or_cfg_decode.or(decode.map(|decode| decode.with_env_or_config(idx)));
                    env_or_cfg_rate_limit_on_error_count =
                        env_or_cfg_rate_limit_on_error_count.or(rate_limit_on_error_count);
                    env_or_cfg_rate_limit_on_error_window_size_secs =
                        env_or_cfg_rate_limit_on_error_window_size_secs
                            .or(rate_limit_on_error_window_size_secs);
                }
                _ => {}
            }

            let into_token = || {
                Ok(Self::Token {
                    token: env_or_cfg_token.with_context(|| format!(
                        "{sccache_dist_auth_token_env} must be set when {sccache_dist_auth_type_env}=token",
                    ))?,
                })
            };

            let into_jwt_validate = || {
                Ok(Self::JwtValidate {
                    audience: env_or_cfg_audience.with_context(|| format!("{sccache_dist_auth_audience_env} must be set when {sccache_dist_auth_type_env}=jwt_validate"))?,
                    issuer: env_or_cfg_issuer.with_context(|| format!("{sccache_dist_auth_issuer_env} must be set when {sccache_dist_auth_type_env}=jwt_validate"))?,
                    jwks_url: env_or_cfg_jwks_url.with_context(|| format!("{sccache_dist_auth_jwks_url_env} must be set when {sccache_dist_auth_type_env}=jwt_validate"))?,
                    claims: env_or_cfg_claims,
                    leeway: env_or_cfg_leeway,
                })
            };

            let into_proxy_token = || {
                Ok(Self::ProxyToken {
                    url: env_or_cfg_url.with_context(|| format!(
                        "{sccache_dist_auth_url_env} must be set when {sccache_dist_auth_type_env}=proxy_token",
                    ))?,
                    cache_secs: env_or_cfg_cache_secs.or(Some(300)),
                    decode: env_or_cfg_decode,
                    rate_limit_on_error_count: env_or_cfg_rate_limit_on_error_count,
                    rate_limit_on_error_window_size_secs:
                        env_or_cfg_rate_limit_on_error_window_size_secs,
                })
            };

            if let Ok(auth_type) = env::var(&sccache_dist_auth_type_env).as_deref() {
                match auth_type {
                    "token" => into_token(),
                    "jwt_validate" => into_jwt_validate(),
                    "proxy_token" => into_proxy_token(),
                    _ => Ok(Self::Insecure),
                }
            } else {
                match self {
                    Self::Token { .. } => into_token(),
                    Self::JwtValidate { .. } => into_jwt_validate(),
                    Self::ProxyToken { .. } => into_proxy_token(),
                    _ => Ok(Self::Insecure),
                }
            }
        }

        fn string_or_list<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            use serde::Deserialize;
            use serde::de::{self, SeqAccess, Visitor};
            use std::fmt;

            struct StringOrList;

            impl<'de> Visitor<'de> for StringOrList {
                type Value = Vec<String>;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("string or list of strings")
                }

                fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    Ok(vec![value.to_owned()])
                }

                fn visit_seq<A>(self, seq: A) -> std::result::Result<Self::Value, A::Error>
                where
                    A: SeqAccess<'de>,
                {
                    Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
                }
            }

            deserializer.deserialize_any(StringOrList)
        }
    }

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum ProxyTokenDecodeConfig {
        #[default]
        None,
        #[serde(rename = "jwt")]
        JwtDecoder {
            #[serde(deserialize_with = "ClientAuth::string_or_list")]
            audience: Vec<String>,
            #[serde(deserialize_with = "ClientAuth::string_or_list")]
            issuer: Vec<String>,
            #[serde(deserialize_with = "ClientAuth::string_or_list")]
            jwks_url: Vec<String>,
            claims: Option<HashMap<String, String>>,
            leeway: Option<u64>,
        },
    }

    impl ProxyTokenDecodeConfig {
        pub fn from_env(idx: Option<usize>) -> Option<Self> {
            let suf = idx.map(|idx| format!("_{idx}")).unwrap_or_default();
            let sccache_dist_auth_proxy_token_decode_env =
                format!("SCCACHE_DIST_AUTH_PROXY_TOKEN_DECODE{suf}");
            let sccache_dist_auth_audience_env = format!("SCCACHE_DIST_AUTH_AUDIENCE{suf}");
            let sccache_dist_auth_issuer_env = format!("SCCACHE_DIST_AUTH_ISSUER{suf}");
            let sccache_dist_auth_jwks_url_env = format!("SCCACHE_DIST_AUTH_JWKS_URL{suf}");
            let sccache_dist_auth_claims_env = format!("SCCACHE_DIST_AUTH_CLAIMS{suf}");
            let sccache_dist_auth_leeway_env = format!("SCCACHE_DIST_AUTH_LEEWAY{suf}");

            match env::var(&sccache_dist_auth_proxy_token_decode_env).as_deref() {
                Ok("jwt") => Some(Self::JwtDecoder {
                    audience: env::var(&sccache_dist_auth_audience_env)
                        .map(|audiences| {
                            audiences
                                .split(" ")
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        })
                        .ok()
                        .unwrap_or_default(),
                    issuer: env::var(&sccache_dist_auth_issuer_env)
                        .map(|issuers| {
                            issuers
                                .split(" ")
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        })
                        .ok()
                        .unwrap_or_default(),
                    jwks_url: env::var(&sccache_dist_auth_jwks_url_env)
                        .map(|jwks_urls| {
                            jwks_urls
                                .split(" ")
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        })
                        .ok()
                        .unwrap_or_default(),
                    claims: env::var(&sccache_dist_auth_claims_env)
                        .map(|claims| {
                            claims
                                .split(" ")
                                .map(|pair| {
                                    pair.split_once("=")
                                        .map(|(key, val)| (key.to_string(), val.to_string()))
                                        .unwrap_or_else(|| (pair.to_string(), "*".to_string()))
                                })
                                .collect::<_>()
                        })
                        .ok(),
                    leeway: number_from_env_var(&sccache_dist_auth_leeway_env).and_then(|s| s.ok()),
                }),
                _ => None,
            }
        }
        pub fn with_env_or_config(self, idx: Option<usize>) -> Self {
            let suf = idx.map(|idx| format!("_{idx}")).unwrap_or_default();
            let sccache_dist_auth_proxy_token_decode_env =
                format!("SCCACHE_DIST_AUTH_PROXY_TOKEN_DECODE{suf}");
            let sccache_dist_auth_audience_env = format!("SCCACHE_DIST_AUTH_AUDIENCE{suf}");
            let sccache_dist_auth_issuer_env = format!("SCCACHE_DIST_AUTH_ISSUER{suf}");
            let sccache_dist_auth_jwks_url_env = format!("SCCACHE_DIST_AUTH_JWKS_URL{suf}");
            let sccache_dist_auth_claims_env = format!("SCCACHE_DIST_AUTH_CLAIMS{suf}");
            let sccache_dist_auth_leeway_env = format!("SCCACHE_DIST_AUTH_LEEWAY{suf}");
            match self {
                Self::JwtDecoder {
                    audience,
                    claims,
                    issuer,
                    jwks_url,
                    leeway,
                } => {
                    let audience = env::var(&sccache_dist_auth_audience_env)
                        .map(|audiences| {
                            audiences
                                .split(" ")
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        })
                        .ok()
                        .or((!audience.is_empty()).then_some(audience));

                    let issuer = env::var(&sccache_dist_auth_issuer_env)
                        .map(|issuers| {
                            issuers
                                .split(" ")
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        })
                        .ok()
                        .or((!issuer.is_empty()).then_some(issuer));

                    let jwks_url = env::var(&sccache_dist_auth_jwks_url_env)
                        .map(|jwks_urls| {
                            jwks_urls
                                .split(" ")
                                .map(|s| s.to_string())
                                .collect::<Vec<_>>()
                        })
                        .ok()
                        .or((!jwks_url.is_empty()).then_some(jwks_url));

                    let claims = env::var(&sccache_dist_auth_claims_env)
                        .map(|claims| {
                            claims
                                .split(" ")
                                .map(|pair| {
                                    pair.split_once("=")
                                        .map(|(key, val)| (key.to_string(), val.to_string()))
                                        .unwrap_or_else(|| (pair.to_string(), "*".to_string()))
                                })
                                .collect::<_>()
                        })
                        .ok()
                        .or(claims);

                    let leeway = number_from_env_var(&sccache_dist_auth_leeway_env)
                        .and_then(|s| s.ok())
                        .or(leeway);

                    Self::JwtDecoder {
                        audience: audience.unwrap_or_else(|| panic!("{sccache_dist_auth_audience_env} must be set when {sccache_dist_auth_proxy_token_decode_env}=jwt")),
                        claims,
                        issuer: issuer.unwrap_or_else(|| panic!("{sccache_dist_auth_issuer_env} must be set when {sccache_dist_auth_proxy_token_decode_env}=jwt")),
                        jwks_url: jwks_url.unwrap_or_else(|| panic!("{sccache_dist_auth_jwks_url_env} must be set when {sccache_dist_auth_proxy_token_decode_env}=jwt")),
                        leeway,
                    }
                }
                _ => Self::None,
            }
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    #[serde(deny_unknown_fields)]
    pub struct FileConfig {
        pub client_auth: Option<Vec<ClientAuth>>,
        #[serde(default = "Config::default_heartbeat_interval_ms")]
        pub heartbeat_interval_ms: u64,
        #[serde(default = "Config::default_job_time_limit")]
        pub job_time_limit: u32,
        #[serde(default = "CacheConfigs::default")]
        pub jobs: CacheConfigs,
        pub keepalive: DistNetworkingKeepalive,
        #[serde(default = "Config::default_max_body_size")]
        pub max_body_size: usize,
        pub max_concurrent_streams: Option<u32>,
        pub message_broker: Option<MessageBroker>,
        pub metrics: MetricsConfigs,
        #[serde(default = "Config::default_public_addr")]
        pub public_addr: SocketAddr,
        #[serde(default = "Config::default_scheduler_id")]
        pub scheduler_id: String,
        #[serde(default = "Config::default_shutdown_timeout")]
        pub shutdown_timeout: u64,
        #[serde(default = "CacheConfigs::default")]
        pub toolchains: CacheConfigs,
    }

    impl Default for FileConfig {
        fn default() -> Self {
            Self {
                client_auth: Some(vec![ClientAuth::Insecure]),
                heartbeat_interval_ms: Config::default_heartbeat_interval_ms(),
                job_time_limit: Config::default_job_time_limit(),
                jobs: CacheConfigs::default(),
                keepalive: Default::default(),
                max_body_size: Config::default_max_body_size(),
                max_concurrent_streams: None,
                message_broker: None,
                metrics: Default::default(),
                public_addr: SocketAddr::from_str("0.0.0.0:10500").unwrap(),
                scheduler_id: Config::default_scheduler_id(),
                shutdown_timeout: Config::default_shutdown_timeout(),
                toolchains: CacheConfigs::default(),
            }
        }
    }

    #[derive(Debug)]
    pub struct Config {
        pub client_auth: Vec<ClientAuth>,
        pub heartbeat_interval_ms: u64,
        pub job_time_limit: u32,
        pub jobs: CacheConfigs,
        pub keepalive: DistNetworkingKeepalive,
        pub max_body_size: usize,
        pub max_concurrent_streams: Option<u32>,
        pub message_broker: Option<MessageBroker>,
        pub metrics: MetricsConfigs,
        pub public_addr: SocketAddr,
        pub scheduler_id: String,
        pub shutdown_timeout: u64,
        pub toolchains: CacheConfigs,
    }

    impl Default for Config {
        fn default() -> Self {
            FileConfig::default().into()
        }
    }

    impl Config {
        // Default to 15s
        fn default_heartbeat_interval_ms() -> u64 {
            15000
        }
        pub fn default_job_time_limit() -> u32 {
            600
        }
        pub fn default_max_body_size() -> usize {
            // 1GiB should be enough for toolchains and compile inputs, right?
            1024 * 1024 * 1024
        }
        pub fn default_public_addr() -> SocketAddr {
            SocketAddr::from_str("0.0.0.0:10500").unwrap()
        }
        pub fn default_scheduler_id() -> String {
            uuid::Uuid::new_v4().as_simple().to_string()
        }
        pub fn default_shutdown_timeout() -> u64 {
            10
        }
        pub fn load(conf_path: Option<PathBuf>) -> Result<Self> {
            let FileConfig {
                client_auth,
                heartbeat_interval_ms,
                job_time_limit,
                jobs,
                keepalive,
                max_body_size,
                max_concurrent_streams,
                message_broker,
                metrics,
                public_addr,
                scheduler_id,
                shutdown_timeout,
                toolchains,
            } = conf_path
                .map(|path| {
                    let conf = try_read_config_file::<FileConfig>(&path)
                        .context("Failed to load scheduler config file");
                    match conf {
                        Ok(conf) => conf.unwrap_or_default(),
                        Err(err) => {
                            warn!("{err:?}");
                            Default::default()
                        }
                    }
                })
                .unwrap_or_default();

            let client_auth = {
                let client_auth = client_auth.unwrap_or_default();
                let client_auth = if client_auth.is_empty() {
                    vec![ClientAuth::Insecure]
                } else {
                    client_auth
                };

                let client_auth_from_env_count = env::vars()
                    .filter(|(key, _)| key.starts_with("SCCACHE_DIST_AUTH_TYPE"))
                    .count();

                let client_auth = if client_auth_from_env_count > client_auth.len() {
                    let num_from_env = client_auth_from_env_count - client_auth.len();
                    [
                        client_auth,
                        (0..num_from_env)
                            .map(|_| ClientAuth::Insecure)
                            .collect::<Vec<_>>(),
                    ]
                    .concat()
                } else {
                    client_auth
                };

                client_auth
                    .into_iter()
                    .enumerate()
                    .map(|(idx, cfg)| cfg.with_env_or_config((idx > 0).then_some(idx)))
                    .try_collect()?
            };

            let jobs = CacheConfigs::default()
                .merge(jobs)
                .merge(config_from_env("SCCACHE_DIST_JOBS_")?.cache);

            let toolchains = CacheConfigs::default()
                .merge(toolchains)
                .merge(config_from_env("SCCACHE_DIST_TOOLCHAINS_")?.cache);

            let heartbeat_interval_ms =
                number_from_env_var("SCCACHE_DIST_SCHEDULER_HEARTBEAT_INTERVAL")
                    .transpose()?
                    .unwrap_or(heartbeat_interval_ms);

            let job_time_limit = number_from_env_var("SCCACHE_DIST_JOB_TIME_LIMIT_SECS")
                .transpose()?
                .unwrap_or(job_time_limit);

            let max_body_size = number_from_env_var("SCCACHE_DIST_MAX_BODY_SIZE")
                .transpose()?
                .unwrap_or(max_body_size);

            let max_concurrent_streams = number_from_env_var("SCCACHE_DIST_MAX_CONCURRENT_STREAMS")
                .transpose()
                .unwrap_or(max_concurrent_streams);

            let message_broker = MessageBroker::from_env().or(message_broker);

            let metrics = metrics.with_env_or_config();

            let public_addr = env::var("SCCACHE_DIST_PUBLIC_ADDR")
                .ok()
                .and_then(|addr| SocketAddr::from_str(&addr).ok())
                .unwrap_or(public_addr);

            let scheduler_id = env::var("SCCACHE_DIST_SCHEDULER_ID")
                .ok()
                .unwrap_or(scheduler_id);

            let shutdown_timeout = number_from_env_var("SCCACHE_DIST_SHUTDOWN_TIMEOUT")
                .transpose()?
                .unwrap_or(shutdown_timeout);

            Ok(Self {
                client_auth,
                heartbeat_interval_ms,
                job_time_limit,
                jobs,
                keepalive: keepalive.with_env_or_config(),
                max_body_size,
                max_concurrent_streams,
                message_broker,
                metrics,
                public_addr,
                scheduler_id,
                shutdown_timeout,
                toolchains,
            })
        }

        pub fn into_file(self) -> FileConfig {
            self.into()
        }
    }

    impl From<Config> for FileConfig {
        fn from(scheduler_config: Config) -> Self {
            Self {
                client_auth: Some(scheduler_config.client_auth),
                heartbeat_interval_ms: scheduler_config.heartbeat_interval_ms,
                job_time_limit: scheduler_config.job_time_limit,
                jobs: scheduler_config.jobs,
                keepalive: scheduler_config.keepalive,
                max_body_size: scheduler_config.max_body_size,
                max_concurrent_streams: scheduler_config.max_concurrent_streams,
                message_broker: scheduler_config.message_broker,
                metrics: scheduler_config.metrics,
                public_addr: scheduler_config.public_addr,
                scheduler_id: scheduler_config.scheduler_id,
                shutdown_timeout: scheduler_config.shutdown_timeout,
                toolchains: scheduler_config.toolchains,
            }
        }
    }

    impl From<FileConfig> for Config {
        fn from(scheduler_config: FileConfig) -> Self {
            Self {
                client_auth: scheduler_config
                    .client_auth
                    .unwrap_or_else(|| vec![ClientAuth::Insecure]),
                heartbeat_interval_ms: scheduler_config.heartbeat_interval_ms,
                job_time_limit: scheduler_config.job_time_limit,
                jobs: scheduler_config.jobs,
                keepalive: scheduler_config.keepalive,
                max_body_size: scheduler_config.max_body_size,
                max_concurrent_streams: scheduler_config.max_concurrent_streams,
                message_broker: scheduler_config.message_broker,
                metrics: scheduler_config.metrics,
                public_addr: scheduler_config.public_addr,
                scheduler_id: scheduler_config.scheduler_id,
                shutdown_timeout: scheduler_config.shutdown_timeout,
                toolchains: scheduler_config.toolchains,
            }
        }
    }
}

#[cfg(feature = "dist-server")]
pub mod server {
    use super::{
        CacheConfigs, MessageBroker, MetricsConfigs, PrometheusMetricsConfig, config_from_env,
        default_disk_cache_dir, number_from_env_var, try_read_config_file,
    };
    use serde::{Deserialize, Serialize};
    use std::{env, net::SocketAddr, path::PathBuf, str::FromStr};

    use crate::errors::*;

    const TEN_GIGS: u64 = 10 * 1024 * 1024 * 1024;
    fn default_toolchain_cache_size() -> u64 {
        TEN_GIGS
    }

    const DEFAULT_POT_CLONE_FROM: &str = "sccache-template";
    const DEFAULT_POT_FS_ROOT: &str = "/opt/pot";
    const DEFAULT_POT_CMD: &str = "pot";
    const DEFAULT_POT_CLONE_ARGS: &[&str] = &["-i", "lo0|127.0.0.2"];

    fn default_pot_clone_from() -> String {
        DEFAULT_POT_CLONE_FROM.to_string()
    }

    fn default_pot_fs_root() -> PathBuf {
        DEFAULT_POT_FS_ROOT.into()
    }

    fn default_pot_cmd() -> PathBuf {
        DEFAULT_POT_CMD.into()
    }

    fn default_pot_clone_args() -> Vec<String> {
        DEFAULT_POT_CLONE_ARGS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[serde(deny_unknown_fields)]
    pub enum BuilderType {
        #[default]
        #[serde(rename = "docker")]
        Docker,
        #[serde(rename = "overlay")]
        Overlay {
            build_dir: PathBuf,
            bwrap_path: PathBuf,
        },
        #[serde(rename = "pot")]
        Pot {
            #[serde(default = "default_pot_fs_root")]
            pot_fs_root: PathBuf,
            #[serde(default = "default_pot_clone_from")]
            clone_from: String,
            #[serde(default = "default_pot_cmd")]
            pot_cmd: PathBuf,
            #[serde(default = "default_pot_clone_args")]
            pot_clone_args: Vec<String>,
        },
    }

    impl BuilderType {
        pub fn with_env_or_config(self) -> Self {
            let mut overlay_build_dir = None;
            let mut overlay_bwrap_path = None;
            let mut pot_clone_from = None;
            let mut pot_clone_args = None;
            let mut pot_cmd = None;
            let mut pot_fs_root = None;

            match self {
                BuilderType::Overlay {
                    build_dir,
                    bwrap_path,
                } => {
                    overlay_build_dir = Some(build_dir);
                    overlay_bwrap_path = Some(bwrap_path);
                }
                BuilderType::Pot {
                    pot_fs_root: fs_root,
                    clone_from,
                    pot_cmd: cmd,
                    pot_clone_args: clone_args,
                } => {
                    pot_fs_root = Some(fs_root);
                    pot_clone_from = Some(clone_from);
                    pot_cmd = Some(cmd);
                    pot_clone_args = Some(clone_args);
                }
                _ => {}
            }

            match env::var("SCCACHE_DIST_BUILDER_TYPE")
                .ok()
                .as_deref()
                .or(overlay_build_dir.as_ref().map(|_| "overlay"))
                .or(pot_clone_from.as_ref().map(|_| "pot"))
            {
                Some("overlay") => BuilderType::Overlay {
                    build_dir: env::var_os("SCCACHE_DIST_BUILD_DIR")
                        .map(Into::into)
                        .or(overlay_build_dir)
                        .unwrap(),
                    bwrap_path: env::var_os("SCCACHE_DIST_BWRAP_PATH")
                        .map(Into::into)
                        .or(overlay_bwrap_path)
                        .unwrap(),
                },
                Some("pot") => BuilderType::Pot {
                    pot_fs_root: env::var_os("SCCACHE_DIST_BUILD_DIR")
                        .map(Into::into)
                        .or(pot_fs_root)
                        .unwrap_or_else(default_pot_fs_root),
                    clone_from: env::var("SCCACHE_DIST_POT_CLONE_FROM")
                        .ok()
                        .or(pot_clone_from)
                        .unwrap_or_else(default_pot_clone_from),
                    pot_cmd: env::var_os("SCCACHE_DIST_POT_CMD")
                        .map(Into::into)
                        .or(pot_cmd)
                        .unwrap_or_else(default_pot_cmd),
                    pot_clone_args: env::var("SCCACHE_DIST_POT_CLONE_ARGS")
                        .ok()
                        .as_deref()
                        .and_then(shlex::split)
                        .or(pot_clone_args)
                        .unwrap_or_else(default_pot_clone_args),
                },
                _ => BuilderType::Docker,
            }
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(default)]
    #[serde(deny_unknown_fields)]
    pub struct FileConfig {
        pub builder: BuilderType,
        #[serde(default = "Config::default_cache_dir")]
        pub cache_dir: PathBuf,
        pub health_check_bind_addr: Option<SocketAddr>,
        #[serde(default = "Config::default_heartbeat_interval_ms")]
        pub heartbeat_interval_ms: u64,
        #[serde(default = "CacheConfigs::default")]
        pub jobs: CacheConfigs,
        #[serde(default = "Config::default_max_per_core_load")]
        pub max_per_core_load: f64,
        #[serde(default = "Config::default_max_per_core_prefetch")]
        pub max_per_core_prefetch: f64,
        pub message_broker: Option<MessageBroker>,
        pub metrics: MetricsConfigs,
        #[serde(default = "Config::default_server_id")]
        pub server_id: String,
        #[serde(default = "Config::default_shutdown_timeout")]
        pub shutdown_timeout: u64,
        #[serde(default = "Config::default_toolchain_cache_size")]
        pub toolchain_cache_size: u64,
        #[serde(default = "CacheConfigs::default")]
        pub toolchains: CacheConfigs,
    }

    impl Default for FileConfig {
        fn default() -> Self {
            Self {
                builder: BuilderType::Docker,
                cache_dir: Config::default_cache_dir(),
                health_check_bind_addr: None,
                heartbeat_interval_ms: Config::default_heartbeat_interval_ms(),
                jobs: CacheConfigs::default(),
                max_per_core_load: Config::default_max_per_core_load(),
                max_per_core_prefetch: Config::default_max_per_core_prefetch(),
                message_broker: None,
                metrics: Default::default(),
                server_id: Config::default_server_id(),
                shutdown_timeout: Config::default_shutdown_timeout(),
                toolchain_cache_size: Config::default_toolchain_cache_size(),
                toolchains: CacheConfigs::default(),
            }
        }
    }

    #[derive(Debug)]
    pub struct Config {
        pub builder: BuilderType,
        pub cache_dir: PathBuf,
        pub health_check_bind_addr: Option<SocketAddr>,
        pub heartbeat_interval_ms: u64,
        pub jobs: CacheConfigs,
        pub max_per_core_load: f64,
        pub max_per_core_prefetch: f64,
        pub message_broker: Option<MessageBroker>,
        pub metrics: MetricsConfigs,
        pub server_id: String,
        pub shutdown_timeout: u64,
        pub toolchain_cache_size: u64,
        pub toolchains: CacheConfigs,
    }

    impl Default for Config {
        fn default() -> Self {
            FileConfig::default().into()
        }
    }

    impl Config {
        pub fn default_cache_dir() -> PathBuf {
            default_disk_cache_dir()
        }
        // Default to 15s
        fn default_heartbeat_interval_ms() -> u64 {
            15000
        }
        // Default to 1
        fn default_max_per_core_load() -> f64 {
            1.0
        }
        // Default to 1
        fn default_max_per_core_prefetch() -> f64 {
            1.0
        }
        fn default_server_id() -> String {
            uuid::Uuid::new_v4().as_simple().to_string()
        }
        fn default_shutdown_timeout() -> u64 {
            10
        }
        fn default_toolchain_cache_size() -> u64 {
            default_toolchain_cache_size()
        }
        pub fn load(conf_path: Option<PathBuf>) -> Result<Self> {
            let FileConfig {
                message_broker,
                builder,
                cache_dir,
                health_check_bind_addr,
                heartbeat_interval_ms,
                jobs,
                max_per_core_load,
                max_per_core_prefetch,
                metrics,
                server_id,
                shutdown_timeout,
                toolchain_cache_size,
                toolchains,
            } = conf_path
                .map(|path| {
                    let conf = try_read_config_file::<FileConfig>(&path)
                        .context("Failed to load server config file");
                    match conf {
                        Ok(conf) => conf.unwrap_or_default(),
                        Err(err) => {
                            warn!("{err:?}");
                            Default::default()
                        }
                    }
                })
                .unwrap_or_default();

            let metrics = metrics.with_env_or_config();

            if let Some(PrometheusMetricsConfig::ListenPath { .. }) = metrics.prometheus {
                return Err(anyhow!(
                    "Invalid config `metrics.prometheus.type=\"path\"`. Choose `type = \"addr\"` or `type = \"push\"`"
                ));
            }

            let builder = builder.with_env_or_config();

            let cache_dir = env::var("SCCACHE_DIST_SERVER_CACHE_DIR")
                .map(PathBuf::from)
                .unwrap_or(cache_dir);

            let jobs = CacheConfigs::default()
                .merge(jobs)
                .merge(config_from_env("SCCACHE_DIST_JOBS_")?.cache);

            let toolchains = CacheConfigs::default()
                .merge(toolchains)
                .merge(config_from_env("SCCACHE_DIST_TOOLCHAINS_")?.cache);

            let health_check_bind_addr = env::var("SCCACHE_DIST_HEALTH_CHECK_BIND_ADDR")
                .ok()
                .and_then(|addr| std::net::SocketAddr::from_str(&addr).ok())
                .or(health_check_bind_addr);

            let heartbeat_interval_ms =
                number_from_env_var("SCCACHE_DIST_SERVER_HEARTBEAT_INTERVAL")
                    .transpose()?
                    .unwrap_or(heartbeat_interval_ms);

            let max_per_core_load = number_from_env_var("SCCACHE_DIST_MAX_PER_CORE_LOAD")
                .transpose()?
                .unwrap_or(max_per_core_load);

            let max_per_core_prefetch = number_from_env_var("SCCACHE_DIST_MAX_PER_CORE_PREFETCH")
                .transpose()?
                .unwrap_or(max_per_core_prefetch);

            let message_broker = MessageBroker::from_env().or(message_broker);

            let server_id = env::var("SCCACHE_DIST_SERVER_ID").ok().unwrap_or(server_id);

            let shutdown_timeout = number_from_env_var("SCCACHE_DIST_SHUTDOWN_TIMEOUT")
                .transpose()?
                .unwrap_or(shutdown_timeout);

            let toolchain_cache_size = number_from_env_var("SCCACHE_DIST_TOOLCHAIN_CACHE_SIZE")
                .transpose()?
                .unwrap_or(toolchain_cache_size);

            Ok(Self {
                builder,
                cache_dir,
                health_check_bind_addr,
                heartbeat_interval_ms,
                jobs,
                max_per_core_load,
                max_per_core_prefetch,
                message_broker,
                metrics,
                server_id,
                shutdown_timeout,
                toolchain_cache_size,
                toolchains,
            })
        }

        pub fn into_file(self) -> FileConfig {
            self.into()
        }
    }

    impl From<Config> for FileConfig {
        fn from(server_config: Config) -> Self {
            Self {
                builder: server_config.builder,
                cache_dir: server_config.cache_dir,
                health_check_bind_addr: server_config.health_check_bind_addr,
                heartbeat_interval_ms: server_config.heartbeat_interval_ms,
                jobs: server_config.jobs,
                max_per_core_load: server_config.max_per_core_load,
                max_per_core_prefetch: server_config.max_per_core_prefetch,
                message_broker: server_config.message_broker,
                metrics: server_config.metrics,
                server_id: server_config.server_id,
                shutdown_timeout: server_config.shutdown_timeout,
                toolchain_cache_size: server_config.toolchain_cache_size,
                toolchains: server_config.toolchains,
            }
        }
    }

    impl From<FileConfig> for Config {
        fn from(server_config: FileConfig) -> Self {
            Self {
                builder: server_config.builder,
                cache_dir: server_config.cache_dir,
                health_check_bind_addr: server_config.health_check_bind_addr,
                heartbeat_interval_ms: server_config.heartbeat_interval_ms,
                jobs: server_config.jobs,
                max_per_core_load: server_config.max_per_core_load,
                max_per_core_prefetch: server_config.max_per_core_prefetch,
                message_broker: server_config.message_broker,
                metrics: server_config.metrics,
                server_id: server_config.server_id,
                shutdown_timeout: server_config.shutdown_timeout,
                toolchain_cache_size: server_config.toolchain_cache_size,
                toolchains: server_config.toolchains,
            }
        }
    }
}

#[test]
fn test_parse_size() {
    assert_eq!(None, parse_size(""));
    assert_eq!(None, parse_size("bogus value"));
    assert_eq!(Some(100), parse_size("100"));
    assert_eq!(Some(2048), parse_size("2K"));
    assert_eq!(Some(2048), parse_size("2k"));
    assert_eq!(Some(10 * 1024 * 1024), parse_size("10M"));
    assert_eq!(Some(TEN_GIGS), parse_size("10G"));
    assert_eq!(Some(1024 * TEN_GIGS), parse_size("10T"));
}

#[test]
fn test_cache_ordering() {
    let disk = CacheType::Disk(DiskCacheConfig::default());
    let s3 = CacheType::S3(S3CacheConfig::default());
    let redis = CacheType::Redis(RedisCacheConfig::default());
    let memcached = CacheType::Memcached(MemcachedCacheConfig::default());
    let gcs = CacheType::GCS(GCSCacheConfig::default());
    let gha = CacheType::GHA(GHACacheConfig::default());
    let azure = CacheType::Azure(AzureCacheConfig::default());
    let webdav = CacheType::Webdav(WebdavCacheConfig::default());
    let oss = CacheType::OSS(OSSCacheConfig::default());
    let cos = CacheType::COS(COSCacheConfig::default());

    assert_eq!(disk.order(), DiskCacheConfig::default_order());
    assert_eq!(s3.order(), S3CacheConfig::default_order());
    assert_eq!(redis.order(), RedisCacheConfig::default_order());
    assert_eq!(memcached.order(), MemcachedCacheConfig::default_order());
    assert_eq!(gcs.order(), GCSCacheConfig::default_order());
    assert_eq!(gha.order(), GHACacheConfig::default_order());
    assert_eq!(azure.order(), AzureCacheConfig::default_order());
    assert_eq!(webdav.order(), WebdavCacheConfig::default_order());
    assert_eq!(oss.order(), OSSCacheConfig::default_order());
    assert_eq!(cos.order(), COSCacheConfig::default_order());

    let mut caches = vec![
        oss, redis, gha, cos, memcached, gcs, s3, webdav, disk, azure,
    ];
    caches.sort();

    while let Some(cache) = caches.pop() {
        for other in caches.iter() {
            assert_eq!(cache.cmp(other), std::cmp::Ordering::Greater);
        }
    }
}

#[test]
fn test_string_from_env_var() {
    let var_name = "TEST_SCCACHE_VAR";
    for value in [None, Some(""), Some("foo")] {
        match value {
            None => unsafe {
                std::env::remove_var(var_name);
            },
            Some(value) => unsafe {
                std::env::set_var(var_name, value);
            },
        }
        let result = string_from_env_var(var_name);
        unsafe {
            std::env::remove_var(var_name);
        }

        let expected = match value {
            None | Some("") => None,
            Some(value) => Some(value.to_string()),
        };

        assert_eq!(result, expected);
    }
}

#[test]
fn config_overrides() {
    let env_conf = EnvConfig {
        cache: CacheConfigs {
            azure: Some(AzureCacheConfig {
                connection_string: String::new(),
                container: String::new(),
                key_prefix: String::new(),
                rw_mode: CacheModeConfig::ReadWrite,
                ..Default::default()
            }),
            disk: Some(DiskCacheConfig {
                dir: "/env-cache".into(),
                size: 5,
                rw_mode: CacheModeConfig::ReadWrite,
                ..Default::default()
            }),
            redis: Some(RedisCacheConfig {
                endpoint: Some("myotherredisurl".to_owned()),
                ttl: 24 * 3600,
                key_prefix: "/redis/prefix".into(),
                db: 10,
                username: Some("user".to_owned()),
                password: Some("secret".to_owned()),
                ..Default::default()
            }),
            ..Default::default()
        },
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: CacheConfigs {
            disk: Some(DiskCacheConfig {
                dir: "/file-cache".into(),
                size: 15,
                rw_mode: CacheModeConfig::ReadWrite,
                ..Default::default()
            }),
            memcached: Some(MemcachedCacheConfig {
                url: "memurl".to_owned(),
                expiration: 24 * 3600,
                key_prefix: String::new(),
                ..Default::default()
            }),
            redis: Some(RedisCacheConfig {
                url: Some("myredisurl".to_owned()),
                ttl: 25 * 3600,
                key_prefix: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    assert_eq!(
        Config::from_env_and_file_configs(env_conf, file_conf).unwrap(),
        Config {
            caches: CacheConfigs {
                disk: Some(DiskCacheConfig {
                    dir: "/env-cache".into(),
                    size: 5,
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                redis: Some(RedisCacheConfig {
                    endpoint: Some("myotherredisurl".to_owned()),
                    ttl: 24 * 3600,
                    key_prefix: "/redis/prefix".into(),
                    db: 10,
                    username: Some("user".to_owned()),
                    password: Some("secret".to_owned()),
                    ..Default::default()
                }),
                memcached: Some(MemcachedCacheConfig {
                    url: "memurl".to_owned(),
                    expiration: 24 * 3600,
                    key_prefix: String::new(),
                    ..Default::default()
                }),
                azure: Some(AzureCacheConfig {
                    connection_string: String::new(),
                    container: String::new(),
                    key_prefix: String::new(),
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    );
}

#[test]
#[cfg(target_os = "windows")]
fn config_basedirs_overrides() {
    // Test that env variable takes precedence over file config
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: vec!["C:/env/basedir".to_string()].into(),
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["C:/file/basedir".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert_eq!(config.basedirs, vec![b"c:/env/basedir/".to_vec()]);

    // Test that file config is used when env is None
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["C:/file/basedir".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert_eq!(config.basedirs, vec![b"c:/file/basedir/".to_vec()]);

    // Test that env config is used when env is set but empty
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: vec![].into(),
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["C:/file/basedir".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert!(config.basedirs.is_empty());

    // Test that both empty results in empty
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: vec![].into(),
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec![],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert!(config.basedirs.is_empty());
}

#[test]
#[cfg(not(target_os = "windows"))]
fn config_basedirs_overrides() {
    // Test that env variable takes precedence over file config
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: vec!["/env/basedir".to_string()].into(),
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/file/basedir".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert_eq!(config.basedirs, vec![b"/env/basedir/".to_vec()]);

    // Test that file config is used when env is None
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/file/basedir".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert_eq!(config.basedirs, vec![b"/file/basedir/".to_vec()]);

    // Test that env config is used when env is set but empty
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: vec![].into(),
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/file/basedir".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert!(config.basedirs.is_empty());

    // Test that both empty results in empty
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: vec![].into(),
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec![],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert!(config.basedirs.is_empty());
    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec![],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert!(config.basedirs.is_empty());
}

#[test]
#[cfg(not(target_os = "windows"))]
fn test_deserialize_basedirs() {
    // Test array of paths
    let toml = r#"
        basedirs = ["/home/user/project", "/home/user/workspace"]

        [cache.disk]
        dir = "/tmp/cache"
        size = 1073741824

        [dist]
    "#;

    let config: FileConfig = toml::from_str(toml).unwrap();
    assert_eq!(
        config.basedirs,
        vec![
            "/home/user/project".to_string(),
            "/home/user/workspace".to_string()
        ]
    );
}

#[test]
fn test_deserialize_basedirs_missing() {
    // Test no basedirs specified (should default to empty vec)
    let toml = r#"
        [cache.disk]
        dir = "/tmp/cache"
        size = 1073741824

        [dist]
    "#;

    let config: FileConfig = toml::from_str(toml).unwrap();
    assert!(config.basedirs.is_empty());
}

#[test]
fn test_cache_mode_from_env_var() {
    struct CacheEnvCheckInfo {
        env_var_value: Option<String>,
        expected_cache_mode: Option<CacheModeConfig>,
    }

    let env_check_infos = vec![
        CacheEnvCheckInfo {
            env_var_value: None,
            expected_cache_mode: None,
        },
        CacheEnvCheckInfo {
            env_var_value: Some(String::new()),
            expected_cache_mode: None,
        },
        CacheEnvCheckInfo {
            env_var_value: Some("read_only".to_string()),
            expected_cache_mode: Some(CacheModeConfig::ReadOnly),
        },
        CacheEnvCheckInfo {
            env_var_value: Some("read_write".to_string()),
            expected_cache_mode: Some(CacheModeConfig::ReadWrite),
        },
        CacheEnvCheckInfo {
            env_var_value: Some("ReAd_ONly".to_string()),
            expected_cache_mode: Some(CacheModeConfig::ReadOnly),
        },
        CacheEnvCheckInfo {
            env_var_value: Some("read_WRITE".to_string()),
            expected_cache_mode: Some(CacheModeConfig::ReadWrite),
        },
        CacheEnvCheckInfo {
            env_var_value: Some("read_only_typo".to_string()),
            expected_cache_mode: None,
        },
        CacheEnvCheckInfo {
            env_var_value: Some("foo".to_string()),
            expected_cache_mode: None,
        },
        CacheEnvCheckInfo {
            env_var_value: Some("any_unsupported_value".to_string()),
            expected_cache_mode: None,
        },
    ];

    let var_name = "SCCACHE_ANY_CACHE_MODE_ENV_VAR";

    for info in env_check_infos {
        unsafe {
            if let Some(env_var_value) = &info.env_var_value {
                std::env::set_var(var_name, env_var_value);
            } else {
                std::env::remove_var(var_name);
            }
        }
        let cache_mode = cache_mode_from_env_var(var_name);

        unsafe {
            std::env::remove_var(var_name);
        }

        assert_eq!(cache_mode, info.expected_cache_mode);
    }
}

#[test]
#[serial(config_from_env)]
#[cfg(not(target_os = "windows"))]
fn test_env_basedirs_single() {
    unsafe {
        std::env::set_var("SCCACHE_BASEDIRS", "/home/user/project");
    }
    let config = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        config.basedirs.expect("SCCACHE_BASEDIRS is set"),
        vec!["/home/user/project".to_string()]
    );
}

#[test]
#[serial(config_from_env)]
#[cfg(target_os = "windows")]
fn test_env_basedirs_single() {
    unsafe {
        std::env::set_var("SCCACHE_BASEDIRS", "C:/home/user/project");
    }
    let config = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        config.basedirs.expect("SCCACHE_BASEDIRS is set"),
        vec!["C:/home/user/project".to_string()]
    );
}

#[test]
#[serial(config_from_env)]
#[cfg(not(target_os = "windows"))]
fn test_env_basedirs_multiple() {
    unsafe {
        std::env::set_var(
            "SCCACHE_BASEDIRS",
            "/home/user/project:/home/user/workspace",
        );
    }
    let config = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        config.basedirs.expect("SCCACHE_BASEDIRS is set"),
        vec![
            "/home/user/project".to_string(),
            "/home/user/workspace".to_string()
        ]
    );
}

#[test]
#[serial(config_from_env)]
#[cfg(target_os = "windows")]
fn test_env_basedirs_multiple() {
    unsafe {
        std::env::set_var(
            "SCCACHE_BASEDIRS",
            "C:/home/user/project;C:/home/user/workspace",
        );
    }
    let config = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        config.basedirs.expect("SCCACHE_BASEDIRS is set"),
        vec![
            "C:/home/user/project".to_string(),
            "C:/home/user/workspace".to_string()
        ]
    );
}

#[test]
#[serial(config_from_env)]
#[cfg(not(target_os = "windows"))]
fn test_env_basedirs_with_spaces() {
    // Test that spaces around paths are not trimmed
    unsafe {
        std::env::set_var(
            "SCCACHE_BASEDIRS",
            " /home/user/project : /home/user/workspace ",
        );
    }
    let env_conf = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        env_conf.basedirs.clone().expect("SCCACHE_BASEDIRS is set"),
        vec![
            " /home/user/project ".to_string(),
            " /home/user/workspace ".to_string()
        ]
    );
    // The lead to trailing spaces are preserved and server fails to start
    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec![],
        client_side_mode: false,
    };
    Config::from_env_and_file_configs(env_conf, file_conf)
        .expect_err("Should fail due to non-absolute path");
}

#[test]
#[serial(config_from_env)]
#[cfg(target_os = "windows")]
fn test_env_basedirs_with_spaces() {
    // Test that spaces around paths are not trimmed
    unsafe {
        std::env::set_var(
            "SCCACHE_BASEDIRS",
            " C:/home/user/project ; C:/home/user/workspace ",
        );
    }
    let env_conf = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        env_conf.basedirs.clone().expect("SCCACHE_BASEDIRS is set"),
        vec![
            " C:/home/user/project ".to_string(),
            " C:/home/user/workspace ".to_string()
        ]
    );
    // The lead to trailing spaces are preserved and server fails to start
    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec![],
        client_side_mode: false,
    };
    Config::from_env_and_file_configs(env_conf, file_conf)
        .expect_err("Should fail due to non-absolute path");
}

#[test]
#[serial(config_from_env)]
#[cfg(not(target_os = "windows"))]
fn test_env_basedirs_empty_entries() {
    // Test that empty entries are filtered out
    unsafe {
        std::env::set_var(
            "SCCACHE_BASEDIRS",
            "/home/user/project::/home/user/workspace",
        );
    }
    let config = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        config.basedirs.expect("SCCACHE_BASEDIRS is set"),
        vec![
            "/home/user/project".to_string(),
            "/home/user/workspace".to_string()
        ]
    );
}

#[test]
#[serial(config_from_env)]
#[cfg(target_os = "windows")]
fn test_env_basedirs_empty_entries() {
    // Test that empty entries are filtered out
    unsafe {
        std::env::set_var(
            "SCCACHE_BASEDIRS",
            "c:/home/user/project;;c:/home/user/workspace",
        );
    }
    let config = config_from_env("SCCACHE_").unwrap();
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }

    assert_eq!(
        config.basedirs.expect("SCCACHE_BASEDIRS is set"),
        vec![
            "c:/home/user/project".to_string(),
            "c:/home/user/workspace".to_string()
        ]
    );
}

#[test]
#[serial(config_from_env)]
fn test_env_basedirs_not_set() {
    unsafe {
        std::env::remove_var("SCCACHE_BASEDIRS");
    }
    let config = config_from_env("SCCACHE_").unwrap();
    assert!(config.basedirs.is_none());
}

#[test]
#[serial(config_from_env)]
#[cfg(feature = "s3")]
fn test_s3_no_credentials_conflict() {
    unsafe {
        env::set_var("SCCACHE_S3_NO_CREDENTIALS", "true");
        env::set_var("SCCACHE_BUCKET", "my-bucket");
        env::set_var("AWS_ACCESS_KEY_ID", "aws-access-key-id");
        env::set_var("AWS_SECRET_ACCESS_KEY", "aws-secret-access-key");
    }

    let cfg = config_from_env("SCCACHE_");

    unsafe {
        env::remove_var("SCCACHE_S3_NO_CREDENTIALS");
        env::remove_var("SCCACHE_BUCKET");
        env::remove_var("AWS_ACCESS_KEY_ID");
        env::remove_var("AWS_SECRET_ACCESS_KEY");
    }

    let error = cfg.unwrap_err();
    assert_eq!(
        "If setting S3 credentials, SCCACHE_S3_NO_CREDENTIALS must not be set.",
        error.to_string()
    );
}

#[test]
#[serial(config_from_env)]
fn test_s3_no_credentials_invalid() {
    unsafe {
        env::set_var("SCCACHE_S3_NO_CREDENTIALS", "yes");
        env::set_var("SCCACHE_BUCKET", "my-bucket");
    }

    let cfg = config_from_env("SCCACHE_");

    unsafe {
        env::remove_var("SCCACHE_S3_NO_CREDENTIALS");
        env::remove_var("SCCACHE_BUCKET");
    }

    let error = cfg.unwrap_err();
    assert_eq!(
        "SCCACHE_S3_NO_CREDENTIALS must be 'true', 'on', '1', 'false', 'off' or '0'.",
        error.to_string()
    );
}

#[test]
#[serial(config_from_env)]
fn test_s3_no_credentials_valid_true() {
    unsafe {
        env::set_var("SCCACHE_S3_NO_CREDENTIALS", "true");
        env::set_var("SCCACHE_BUCKET", "my-bucket");
    }

    let cfg = config_from_env("SCCACHE_");

    unsafe {
        env::remove_var("SCCACHE_S3_NO_CREDENTIALS");
        env::remove_var("SCCACHE_BUCKET");
    }

    let env_cfg = cfg.unwrap();
    match env_cfg.cache.s3 {
        Some(S3CacheConfig {
            ref bucket,
            no_credentials,
            ..
        }) => {
            assert_eq!(bucket, "my-bucket");
            assert!(no_credentials);
        }
        None => unreachable!(),
    }
}

#[test]
#[serial(config_from_env)]
fn test_s3_no_credentials_valid_false() {
    unsafe {
        env::set_var("SCCACHE_S3_NO_CREDENTIALS", "false");
        env::set_var("SCCACHE_BUCKET", "my-bucket");
    }

    let cfg = config_from_env("SCCACHE_");

    unsafe {
        env::remove_var("SCCACHE_S3_NO_CREDENTIALS");
        env::remove_var("SCCACHE_BUCKET");
    }

    let env_cfg = cfg.unwrap();
    match env_cfg.cache.s3 {
        Some(S3CacheConfig {
            ref bucket,
            no_credentials,
            ..
        }) => {
            assert_eq!(bucket, "my-bucket");
            assert!(!no_credentials);
        }
        None => unreachable!(),
    }
}

#[test]
#[serial(config_from_env)]
#[cfg(feature = "gcs")]
fn test_gcs_service_account() {
    unsafe {
        env::set_var("SCCACHE_S3_NO_CREDENTIALS", "false");
        env::set_var("SCCACHE_GCS_BUCKET", "my-bucket");
        env::set_var("SCCACHE_GCS_SERVICE_ACCOUNT", "my@example.com");
        env::set_var("SCCACHE_GCS_RW_MODE", "READ_WRITE");
    }

    let cfg = config_from_env("SCCACHE_");

    unsafe {
        env::remove_var("SCCACHE_GCS_BUCKET");
        env::remove_var("SCCACHE_GCS_SERVICE_ACCOUNT");
        env::remove_var("SCCACHE_GCS_RW_MODE");
    }

    let env_cfg = cfg.unwrap();
    match env_cfg.cache.gcs {
        Some(GCSCacheConfig {
            ref bucket,
            service_account,
            rw_mode,
            ..
        }) => {
            assert_eq!(bucket, "my-bucket");
            assert_eq!(service_account, Some("my@example.com".to_string()));
            assert_eq!(rw_mode, CacheModeConfig::ReadWrite);
        }
        None => unreachable!(),
    }
}

#[test]
fn client_toml_parse() {
    const CONFIG_STR: &str = r#"
server_startup_timeout_ms = 10000

[dist]
# where to find the scheduler
scheduler_url = "http://1.2.3.4:10600"
# a set of prepackaged toolchains
toolchains = []
# the maximum size of the toolchain cache in bytes
toolchain_cache_size = 5368709120
cache_dir = "/home/user/.cache/sccache-dist-client"

[dist.auth]
type = "token"
token = "secrettoken"

[cache.azure]
connection_string = "connection_string"
container = "container"
key_prefix = "key_prefix"

[cache.disk]
dir = "/tmp/.cache/sccache"
size = 7516192768 # 7 GiBytes

[cache.gcs]
rw_mode = "READ_ONLY"
# rw_mode = "READ_WRITE"
cred_path = "/psst/secret/cred"
bucket = "bucket"
key_prefix = "prefix"
service_account = "example_service_account"

[cache.gha]
enabled = true
version = "sccache"

[cache.memcached]
# Deprecated alias for `endpoint`
# url = "127.0.0.1:11211"
endpoint = "tcp://127.0.0.1:11211"
# Username and password for authentication
username = "user"
password = "passwd"
expiration = 90000
key_prefix = "/custom/prefix/if/need"

[cache.redis]
url = "redis://user:passwd@1.2.3.4:6379/?db=1"
endpoint = "redis://127.0.0.1:6379"
cluster_endpoints = "tcp://10.0.0.1:6379,redis://10.0.0.2:6379"
username = "another_user"
password = "new_passwd"
db = 12
expiration = 86400
key_prefix = "/my/redis/cache"

[cache.s3]
bucket = "name"
region = "us-east-2"
endpoint = "s3-us-east-1.amazonaws.com"
use_ssl = true
key_prefix = "s3prefix"
no_credentials = true
server_side_encryption = false
rw_mode = "READ_WRITE"

[cache.webdav]
endpoint = "http://127.0.0.1:8080"
key_prefix = "webdavprefix"
username = "webdavusername"
password = "webdavpassword"
token = "webdavtoken"

[cache.oss]
bucket = "name"
endpoint = "oss-us-east-1.aliyuncs.com"
key_prefix = "ossprefix"
no_credentials = true
rw_mode = "READ_ONLY"

[cache.cos]
bucket = "name"
endpoint = "cos.na-siliconvalley.myqcloud.com"
key_prefix = "cosprefix"
"#;

    let file_config: FileConfig = toml::from_str(CONFIG_STR).expect("Is valid toml.");
    assert_eq!(
        file_config,
        FileConfig {
            cache: CacheConfigs {
                // azure: None, // TODO not sure how to represent a unit struct in TOML Some(AzureCacheConfig),
                azure: Some(AzureCacheConfig {
                    connection_string: "connection_string".into(),
                    container: "container".into(),
                    key_prefix: "key_prefix".into(),
                    ..Default::default()
                }),
                disk: Some(DiskCacheConfig {
                    dir: PathBuf::from("/tmp/.cache/sccache"),
                    size: 7 * 1024 * 1024 * 1024,
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                gcs: Some(GCSCacheConfig {
                    bucket: "bucket".to_owned(),
                    cred_path: Some("/psst/secret/cred".to_string()),
                    service_account: Some("example_service_account".to_string()),
                    rw_mode: CacheModeConfig::ReadOnly,
                    key_prefix: "prefix".into(),
                    credential_url: None,
                    ..Default::default()
                }),
                gha: Some(GHACacheConfig {
                    enabled: true,
                    version: "sccache".to_string(),
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                redis: Some(RedisCacheConfig {
                    url: Some("redis://user:passwd@1.2.3.4:6379/?db=1".to_owned()),
                    endpoint: Some("redis://127.0.0.1:6379".to_owned()),
                    cluster_endpoints: Some("tcp://10.0.0.1:6379,redis://10.0.0.2:6379".to_owned()),
                    username: Some("another_user".to_owned()),
                    password: Some("new_passwd".to_owned()),
                    db: 12,
                    ttl: 24 * 3600,
                    key_prefix: "/my/redis/cache".into(),
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                memcached: Some(MemcachedCacheConfig {
                    url: "tcp://127.0.0.1:11211".to_owned(),
                    username: Some("user".to_owned()),
                    password: Some("passwd".to_owned()),
                    expiration: 25 * 3600,
                    key_prefix: "/custom/prefix/if/need".into(),
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                s3: Some(S3CacheConfig {
                    bucket: "name".to_owned(),
                    region: Some("us-east-2".to_owned()),
                    endpoint: Some("s3-us-east-1.amazonaws.com".to_owned()),
                    use_ssl: Some(true),
                    key_prefix: "s3prefix".into(),
                    no_credentials: true,
                    server_side_encryption: Some(false),
                    enable_virtual_host_style: None,
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                webdav: Some(WebdavCacheConfig {
                    endpoint: "http://127.0.0.1:8080".to_string(),
                    key_prefix: "webdavprefix".into(),
                    username: Some("webdavusername".to_string()),
                    password: Some("webdavpassword".to_string()),
                    token: Some("webdavtoken".to_string()),
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                oss: Some(OSSCacheConfig {
                    bucket: "name".to_owned(),
                    endpoint: Some("oss-us-east-1.aliyuncs.com".to_owned()),
                    key_prefix: "ossprefix".into(),
                    no_credentials: true,
                    rw_mode: CacheModeConfig::ReadOnly,
                    ..Default::default()
                }),
                cos: Some(COSCacheConfig {
                    bucket: "name".to_owned(),
                    endpoint: Some("cos.na-siliconvalley.myqcloud.com".to_owned()),
                    key_prefix: "cosprefix".into(),
                    rw_mode: CacheModeConfig::ReadWrite,
                    ..Default::default()
                }),
                multilevel: None,
            },
            dist: DistConfig {
                auth: DistAuth::Token {
                    token: "secrettoken".to_owned()
                },
                cache_dir: PathBuf::from("/home/user/.cache/sccache-dist-client"),
                rewrite_includes_only: false,
                toolchains: vec![],
                toolchain_cache_size: 5368709120,
                #[cfg(any(feature = "dist-client", feature = "dist-server"))]
                scheduler_url: Some(
                    parse_http_url("http://1.2.3.4:10600")
                        .map(|url| { HTTPUrl::from_url(url) })
                        .expect("Scheduler url must be valid url str")
                ),
                #[cfg(not(any(feature = "dist-client", feature = "dist-server")))]
                scheduler_url: Some("http://1.2.3.4:10600".to_owned()),
                ..Default::default()
            },
            server_startup_timeout_ms: Some(10000),
            basedirs: vec![],
            client_side_mode: false,
        }
    );
}

#[test]
#[cfg(feature = "dist-server")]
fn scheduler_toml_parse() {
    use std::net::SocketAddr;

    const CONFIG_STR: &str = r#"
# The socket address the scheduler will listen on. It's strongly recommended
# to listen on localhost and put a HTTPS server in front of it.
public_addr = "127.0.0.1:10500"
# The address of the AMQP broker
message_broker.amqp = "amqp://127.0.0.1:5672//"
scheduler_id = "scheduler-1"
# Don't allow jobs to run for longer than 20 minutes
job_time_limit = 1200

[metrics.prometheus]
type = "push"
endpoint = "http://127.0.0.1:9091/metrics/job/scheduler"
interval = 1000
username = "sccache"
password = "sccache"

[jobs.redis]
endpoint = "redis://127.0.0.1:6379"
expiration = 3600
key_prefix = "/sccache-dist-jobs"

[toolchains.s3]
bucket = "sccache"
region = "auto"
endpoint = "192.168.1.69:9000"
use_ssl = false
no_credentials = false
key_prefix = "sccache-dist-toolchains"
"#;

    let scheduler_config: scheduler::FileConfig =
        toml::from_str(CONFIG_STR).expect("Is valid toml.");
    assert_eq!(
        scheduler_config,
        scheduler::FileConfig {
            public_addr: SocketAddr::from_str("127.0.0.1:10500").unwrap(),
            job_time_limit: 1200,
            message_broker: Some(MessageBroker::AMQP("amqp://127.0.0.1:5672//".into())),
            metrics: MetricsConfigs {
                prometheus: Some(PrometheusMetricsConfig::PushGateway {
                    endpoint: "http://127.0.0.1:9091/metrics/job/scheduler".into(),
                    interval: Some(1000),
                    username: Some("sccache".into()),
                    password: Some("sccache".into()),
                    http_method: None,
                    idle_timeout_secs: None,
                }),
                ..Default::default()
            },
            jobs: CacheConfigs {
                redis: Some(RedisCacheConfig {
                    endpoint: Some("redis://127.0.0.1:6379".into()),
                    ttl: 3600,
                    key_prefix: "/sccache-dist-jobs".into(),
                    ..RedisCacheConfig::default()
                }),
                ..CacheConfigs::default()
            },
            toolchains: CacheConfigs {
                s3: Some(S3CacheConfig {
                    bucket: "sccache".into(),
                    region: Some("auto".into()),
                    endpoint: Some("192.168.1.69:9000".into()),
                    use_ssl: Some(false),
                    no_credentials: false,
                    key_prefix: "sccache-dist-toolchains".into(),
                    ..S3CacheConfig::default()
                }),
                ..CacheConfigs::default()
            },
            scheduler_id: "scheduler-1".into(),
            ..Default::default()
        }
    );
}

#[test]
#[cfg(feature = "dist-server")]
fn server_toml_parse() {
    use server::BuilderType;
    const CONFIG_STR: &str = r#"
# This is where client toolchains will be stored.
cache_dir = "/tmp/toolchains"
# Dedicate (nproc * 1.25) CPUs to building
max_per_core_load = 1.25
# Prefetch (nproc * 1) jobs
max_per_core_prefetch = 1
server_id = "server-1"
# The maximum size of the toolchain cache, in bytes.
# If unspecified the default is 10GB.
toolchain_cache_size = 10737418240

[builder]
type = "overlay"
# The directory under which a sandboxed filesystem will be created for builds.
build_dir = "/tmp/build"
# The path to the bubblewrap version 0.3.0+ `bwrap` binary.
bwrap_path = "/usr/bin/bwrap"

[message_broker]
amqp = "amqp://127.0.0.1:5672//"

[metrics.prometheus]
type = "push"
endpoint = "http://127.0.0.1:9091/metrics/job/server"
interval = 1000
username = "sccache"
password = "sccache"

[jobs.redis]
endpoint = "redis://127.0.0.1:6379"
expiration = 3600
key_prefix = "/sccache-dist-jobs"

[toolchains.s3]
bucket = "sccache"
region = "auto"
endpoint = "192.168.1.69:9000"
use_ssl = false
no_credentials = false
key_prefix = "sccache-dist-toolchains"
"#;

    let server_config: server::FileConfig = toml::from_str(CONFIG_STR).expect("Is valid toml.");
    assert_eq!(
        server_config,
        server::FileConfig {
            builder: BuilderType::Overlay {
                build_dir: PathBuf::from("/tmp/build"),
                bwrap_path: PathBuf::from("/usr/bin/bwrap"),
            },
            cache_dir: PathBuf::from("/tmp/toolchains"),
            max_per_core_load: 1.25,
            max_per_core_prefetch: 1.0,
            message_broker: Some(MessageBroker::AMQP("amqp://127.0.0.1:5672//".into())),
            metrics: MetricsConfigs {
                prometheus: Some(PrometheusMetricsConfig::PushGateway {
                    endpoint: "http://127.0.0.1:9091/metrics/job/server".into(),
                    interval: Some(1000),
                    username: Some("sccache".into()),
                    password: Some("sccache".into()),
                    http_method: None,
                    idle_timeout_secs: None,
                }),
                ..Default::default()
            },
            jobs: CacheConfigs {
                redis: Some(RedisCacheConfig {
                    endpoint: Some("redis://127.0.0.1:6379".into()),
                    ttl: 3600,
                    key_prefix: "/sccache-dist-jobs".into(),
                    ..RedisCacheConfig::default()
                }),
                ..CacheConfigs::default()
            },
            toolchains: CacheConfigs {
                s3: Some(S3CacheConfig {
                    bucket: "sccache".into(),
                    region: Some("auto".into()),
                    endpoint: Some("192.168.1.69:9000".into()),
                    use_ssl: Some(false),
                    no_credentials: false,
                    key_prefix: "sccache-dist-toolchains".into(),
                    ..S3CacheConfig::default()
                }),
                ..CacheConfigs::default()
            },
            server_id: "server-1".into(),
            toolchain_cache_size: 10737418240,
            ..Default::default()
        }
    );
}

#[test]
fn human_units_parse() {
    const CONFIG_STR: &str = r#"
[dist]
toolchain_cache_size = "5g"

[cache.disk]
size = "7g"
"#;

    let file_config: FileConfig = toml::from_str(CONFIG_STR).expect("Is valid toml.");
    assert_eq!(
        file_config,
        FileConfig {
            cache: CacheConfigs {
                disk: Some(DiskCacheConfig {
                    size: 7 * 1024 * 1024 * 1024,
                    ..Default::default()
                }),
                ..Default::default()
            },
            dist: DistConfig {
                toolchain_cache_size: 5 * 1024 * 1024 * 1024,
                ..Default::default()
            },
            server_startup_timeout_ms: None,
            basedirs: vec![],
            client_side_mode: false,
        }
    );
}

// Integration tests: Config normalization + strip_basedirs usage

#[test]
#[cfg(not(target_os = "windows"))]
fn test_integration_config_normalizes_and_strips() {
    // Test that Config normalizes basedirs and strip_basedirs uses them correctly
    use crate::util::strip_basedirs;
    use std::borrow::Cow;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/home/user/project".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    // Verify config normalized the basedir with trailing slash
    assert_eq!(config.basedirs, vec![b"/home/user/project/"]);

    // Test that strip_basedirs uses the normalized basedir
    let input = b"# 1 \"/home/user/project/src/main.c\"\nint main() { return 0; }";
    let output = strip_basedirs(input, &config.basedirs);

    // Should strip the basedir
    let expected = b"# 1 \"src/main.c\"\nint main() { return 0; }";
    assert_eq!(&*output, expected);
    assert!(matches!(output, Cow::Owned(_)));
}

#[test]
#[cfg(not(target_os = "windows"))]
fn test_integration_normalized_path_with_double_slashes() {
    // Test that Config normalizes paths with double slashes
    use crate::util::strip_basedirs;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/home//user///project/".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    // Config should normalize to single slashes with one trailing slash
    assert_eq!(config.basedirs, vec![b"/home/user/project/"]);

    // Verify it works with strip_basedirs
    let input = b"# 1 \"/home/user/project/src/main.c\"";
    let output = strip_basedirs(input, &config.basedirs);
    assert_eq!(&*output, b"# 1 \"src/main.c\"");
}

#[test]
#[cfg(target_os = "windows")]
fn test_integration_windows_path_normalization() {
    // Test that Config normalizes Windows paths correctly
    use crate::util::strip_basedirs;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["C:\\Users\\Test\\Project".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    // Should be normalized to lowercase with forward slashes
    assert_eq!(config.basedirs, vec![b"c:/users/test/project/"]);

    // Test with mixed case preprocessor output
    let input = b"# 1 \"C:\\Users\\Test\\Project\\src\\main.c\"";
    let output = strip_basedirs(input, &config.basedirs);
    assert_eq!(&*output, b"# 1 \"src\\main.c\"");
}

#[test]
#[cfg(not(target_os = "windows"))]
fn test_integration_cow_borrowed_when_no_match() {
    // Test that strip_basedirs returns Cow::Borrowed when no stripping occurs
    use crate::util::strip_basedirs;
    use std::borrow::Cow;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/home/user/project".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    // Input doesn't contain the basedir
    let input = b"# 1 \"/other/path/main.c\"\nint main() { return 0; }";
    let output = strip_basedirs(input, &config.basedirs);

    // Should return borrowed reference (no allocation)
    assert!(matches!(output, Cow::Borrowed(_)));
    assert_eq!(&*output, input);
}

#[test]
#[cfg(not(target_os = "windows"))]
fn test_integration_cow_borrowed_when_empty_basedirs() {
    // Test that strip_basedirs returns Cow::Borrowed when basedirs is empty
    use crate::util::strip_basedirs;
    use std::borrow::Cow;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec![],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert!(config.basedirs.is_empty());

    let input = b"# 1 \"/home/user/project/src/main.c\"";
    let output = strip_basedirs(input, &config.basedirs);

    // Should return borrowed reference when basedirs is empty
    assert!(matches!(output, Cow::Borrowed(_)));
    assert_eq!(&*output, input);
}

#[test]
#[cfg(not(target_os = "windows"))]
fn test_integration_multiple_basedirs_longest_match() {
    // Test that strip_basedirs prefers longest match with normalized basedirs
    use crate::util::strip_basedirs;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/home/user".to_string(), "/home/user/project".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    // Both should be normalized with trailing slashes
    assert_eq!(config.basedirs.len(), 2);
    assert_eq!(config.basedirs[0], b"/home/user/");
    assert_eq!(config.basedirs[1], b"/home/user/project/");

    // Input matches both, but longest should win
    let input = b"# 1 \"/home/user/project/src/main.c\"";
    let output = strip_basedirs(input, &config.basedirs);

    // Should match the longest basedir (/home/user/project/)
    let expected = b"# 1 \"src/main.c\"";
    assert_eq!(&*output, expected);
}

#[test]
#[cfg(not(target_os = "windows"))]
fn test_integration_paths_with_dots_normalized() {
    // Test that paths with . and .. are normalized correctly
    use crate::util::strip_basedirs;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["/home/user/./project/../project".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    // Should be normalized to remove ./ and ../
    assert_eq!(config.basedirs[0], b"/home/user/project/");

    // Verify it works with strip_basedirs
    let input = b"# 1 \"/home/user/project/src/main.c\"";
    let output = strip_basedirs(input, &config.basedirs);
    let expected = b"# 1 \"src/main.c\"";
    assert_eq!(&*output, expected);
}

#[test]
#[cfg(target_os = "windows")]
fn test_integration_windows_mixed_slashes() {
    // Test Windows path with mixed slashes in preprocessor output
    use crate::util::strip_basedirs;

    let env_conf = EnvConfig {
        cache: Default::default(),
        basedirs: None,
        client_side_mode: None,
    };

    let file_conf = FileConfig {
        cache: Default::default(),
        dist: Default::default(),
        server_startup_timeout_ms: None,
        basedirs: vec!["C:\\Users\\test\\project".to_string()],
        client_side_mode: false,
    };

    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();
    assert_eq!(config.basedirs[0], b"c:/users/test/project/");

    // Preprocessor output with mixed slashes
    let input = b"# 1 \"C:/Users\\test\\project\\src/main.c\"";
    let output = strip_basedirs(input, &config.basedirs);

    // Should strip despite mixed slashes
    let expected = b"# 1 \"src/main.c\"";
    assert_eq!(&*output, expected);
}

#[test]
#[serial(config_from_env)]
#[cfg(not(target_os = "windows"))]
fn test_integration_env_variable_to_strip() {
    // Test full flow: SCCACHE_BASEDIRS env var -> Config -> strip_basedirs
    use crate::util::strip_basedirs;

    unsafe {
        env::set_var("SCCACHE_BASEDIRS", "/home/user/project:/tmp/build");
    }

    let env_conf = config_from_env("SCCACHE_").unwrap();
    let file_conf = FileConfig::default();
    let config = Config::from_env_and_file_configs(env_conf, file_conf).unwrap();

    unsafe {
        env::remove_var("SCCACHE_BASEDIRS");
    }

    // Should have two normalized basedirs
    assert_eq!(config.basedirs.len(), 2);
    assert_eq!(config.basedirs[0], b"/home/user/project/");
    assert_eq!(config.basedirs[1], b"/tmp/build/");

    // Test stripping with both
    let input1 = b"# 1 \"/home/user/project/src/main.c\"";
    let output1 = strip_basedirs(input1, &config.basedirs);
    assert_eq!(&*output1, b"# 1 \"src/main.c\"");

    let input2 = b"# 1 \"/tmp/build/obj/file.o\"";
    let output2 = strip_basedirs(input2, &config.basedirs);
    assert_eq!(&*output2, b"# 1 \"obj/file.o\"");
}

#[test]
fn test_cache_levels_parsing() {
    // Test parsing cache levels from config
    let config_str = r#"
[cache.disk]
dir = "/tmp/disk"
size = 1024

[cache.s3]
bucket = "my-bucket"
region = "us-west-2"
no_credentials = false

[cache.redis]
endpoint = "redis://localhost"

[cache.multilevel]
chain = ["disk", "redis", "s3"]
"#;

    let file_config: FileConfig = toml::from_str(config_str).expect("Is valid toml");
    assert!(file_config.cache.multilevel.is_some());
    let ml_config = file_config.cache.multilevel.unwrap();
    assert_eq!(ml_config.chain.len(), 3);
    assert_eq!(ml_config.chain[0], "disk");
    assert_eq!(ml_config.chain[1], "redis");
    assert_eq!(ml_config.chain[2], "s3");
}

#[test]
fn test_cache_levels_backward_compatibility() {
    // Test that configs without levels still work (single cache selection)
    let config_str = r#"
[cache.s3]
bucket = "my-bucket"
region = "us-west-2"
no_credentials = false
"#;

    let file_config: FileConfig = toml::from_str(config_str).expect("Is valid toml");
    assert!(file_config.cache.multilevel.is_none());
    assert!(file_config.cache.s3.is_some());
}

#[test]
fn test_get_cache_levels_single_cache() {
    let configs = &CacheConfigs {
        s3: Some(S3CacheConfig {
            bucket: "test".to_string(),
            region: None,
            key_prefix: String::new(),
            no_credentials: false,
            endpoint: None,
            use_ssl: None,
            server_side_encryption: None,
            enable_virtual_host_style: None,
            rw_mode: CacheModeConfig::ReadWrite,
            ..Default::default()
        }),
        ..Default::default()
    };

    let levels: Result<Vec<CacheType>> = configs.into();
    let levels = levels.expect("Should get single cache");
    assert_eq!(levels.len(), 1);
}

#[test]
fn test_get_cache_levels_invalid_level() {
    let configs = &CacheConfigs {
        multilevel: Some(MultiLevelConfig {
            chain: vec!["unknown_cache".to_string()],
            write_error_policy: WriteErrorPolicy::default(),
        }),
        ..Default::default()
    };

    let result: Result<Vec<CacheType>> = configs.into();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Unknown cache level")
    );
}

#[test]
fn test_get_cache_levels_missing_config() {
    let configs = &CacheConfigs {
        multilevel: Some(MultiLevelConfig {
            chain: vec!["s3".to_string()],
            write_error_policy: WriteErrorPolicy::default(),
        }),
        ..Default::default()
    };

    let result: Result<Vec<CacheType>> = configs.into();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("s3 cache not configured")
    );
}
