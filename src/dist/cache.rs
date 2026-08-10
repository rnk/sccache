#[cfg(feature = "dist-client")]
pub use self::client::ClientToolchains;
#[cfg(feature = "dist-server")]
pub use self::server::ServerToolchains;

#[cfg(feature = "dist-client")]
mod client {

    use anyhow::{Context, Error, Result, bail};
    use fs_err as fs;
    use futures::lock::Mutex;
    use std::collections::{HashMap, HashSet};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::{
        config,
        dist::{
            PathTransformer, Toolchain,
            pkg::{PackagedToolchain, ToolchainPackager},
        },
        lru_disk_cache::Error as LruError,
        lru_disk_cache::LruDiskCache,
        util::Digest,
    };

    async fn path_key<T: AsRef<Path>>(path: T) -> Result<String> {
        Digest::from_file(path).await.map(|digest| digest.finish())
    }

    #[derive(Clone, Debug)]
    pub struct CustomToolchain {
        archive: PathBuf,
        compiler_executable: String,
    }

    // TODO: possibly shouldn't be public
    pub struct ClientToolchains {
        cache_dir: PathBuf,
        cache: Mutex<LruDiskCache>,
        // Lookup from dist toolchain -> path to custom toolchain archive
        custom_toolchain_archives: Mutex<HashMap<Toolchain, PathBuf>>,
        // Lookup from local path -> toolchain details
        // The Option<Toolchain> could be populated on startup, but it's lazy for efficiency
        custom_toolchain_paths: Mutex<HashMap<PathBuf, (CustomToolchain, Option<Toolchain>)>>,
        // Toolchains configured to not be distributed
        disabled_toolchains: HashSet<PathBuf>,
        // Toolchains that have been hashed but not yet cached to disk
        pending_toolchains: Mutex<HashMap<String, Arc<dyn PackagedToolchain>>>,
        // Local machine mapping from 'weak' hashes to strong toolchain hashes
        // - Weak hashes are what sccache uses to determine if a compiler has changed
        //   on the local machine - they're fast and 'good enough' (assuming we trust
        //   the local machine), but not safe if other users can update the cache.
        // - Strong hashes (or 'archive ids') are the hash of the complete compiler contents that
        //   will be sent over the wire for use in distributed compilation - it is assumed
        //   that if two of them match, the contents of a compiler archive cannot
        //   have been tampered with
        weak_map: Mutex<HashMap<String, String>>,
    }

    impl ClientToolchains {
        pub fn new(
            cache_dir: &Path,
            cache_size: u64,
            toolchain_configs: &[config::DistToolchainConfig],
        ) -> Result<Self> {
            let cache_dir = cache_dir.to_owned();
            fs::create_dir_all(&cache_dir).context(format!(
                "failed to create top level toolchain cache dir: {}",
                cache_dir.display()
            ))?;

            let weak_map_path = cache_dir.join("weak_map.json");
            if !weak_map_path.exists() {
                fs::File::create(&weak_map_path)
                    .and_then(|mut f| f.write_all(b"{}"))
                    .context(format!(
                        "failed to create new toolchain weak map file: {}",
                        weak_map_path.display()
                    ))?;
            }
            let weak_map = fs::File::open(&weak_map_path)
                .map_err(Error::from)
                .and_then(|f| serde_json::from_reader(f).map_err(Error::from))
                .context(format!(
                    "failed to load toolchain weak map: {}",
                    weak_map_path.display()
                ))?;

            let tc_cache_dir = cache_dir.join("tc");
            let cache = LruDiskCache::new(&tc_cache_dir, cache_size)
                .map(Mutex::new)
                .context("failed to initialise a toolchain cache")?;

            // Load in toolchain configuration
            let mut custom_toolchain_paths = HashMap::new();
            let mut disabled_toolchains = HashSet::new();
            for ct in toolchain_configs.iter() {
                match ct {
                    config::DistToolchainConfig::PathOverride {
                        compiler_executable,
                        archive,
                        archive_compiler_executable,
                    } => {
                        debug!(
                            "Registering custom toolchain for {}",
                            compiler_executable.display()
                        );
                        let custom_tc = CustomToolchain {
                            archive: archive.clone(),
                            compiler_executable: archive_compiler_executable.clone(),
                        };
                        if custom_toolchain_paths
                            .insert(compiler_executable.clone(), (custom_tc, None))
                            .is_some()
                        {
                            bail!("Multiple toolchains for {}", compiler_executable.display())
                        }
                        if disabled_toolchains.contains(compiler_executable) {
                            bail!(
                                "Override for toolchain {} conflicts with it being disabled",
                                compiler_executable.display()
                            )
                        }
                    }
                    config::DistToolchainConfig::NoDist {
                        compiler_executable,
                    } => {
                        debug!("Disabling toolchain {}", compiler_executable.display());
                        if !disabled_toolchains.insert(compiler_executable.clone()) {
                            bail!(
                                "Disabled toolchain {} multiple times",
                                compiler_executable.display()
                            )
                        }
                        if custom_toolchain_paths.contains_key(compiler_executable) {
                            bail!(
                                "Override for toolchain {} conflicts with it being disabled",
                                compiler_executable.display()
                            )
                        }
                    }
                }
            }
            let custom_toolchain_paths = Mutex::new(custom_toolchain_paths);

            Ok(Self {
                cache_dir,
                cache,
                custom_toolchain_archives: Mutex::new(HashMap::new()),
                custom_toolchain_paths,
                disabled_toolchains,
                pending_toolchains: Default::default(),
                // TODO: shouldn't clear on restart, but also should have some
                // form of pruning
                weak_map: Mutex::new(weak_map),
            })
        }

        // Get the bytes of a toolchain tar
        // TODO: by this point the toolchain should be known to exist
        pub async fn get_toolchain(&self, tc: &Toolchain) -> Result<Option<fs::File>> {
            // TODO: be more relaxed about path casing and slashes on Windows
            let file = if let Some(custom_tc_archive) =
                self.custom_toolchain_archives.lock().await.get(tc)
            {
                fs::File::open(custom_tc_archive).with_context(|| {
                    format!(
                        "could not open file for toolchain {}",
                        custom_tc_archive.display()
                    )
                })?
            } else {
                match self.cache.lock().await.get_file(&tc.archive_id) {
                    Ok(file) => file,
                    Err(LruError::FileNotInCache) => return Ok(None),
                    Err(e) => return Err(e).context("error while retrieving toolchain from cache"),
                }
            };
            Ok(Some(file))
        }

        // Get the toolchain hash from the cache, or hash it and insert it into the cache
        pub async fn hash_toolchain(
            &self,
            compiler_path: &Path,
            weak_toolchain_key: &str,
            toolchain_packager: Box<dyn ToolchainPackager>,
            path_transformer: &mut PathTransformer,
        ) -> Result<(
            Toolchain,
            Option<(String, PathBuf)>,
            Option<Arc<dyn PackagedToolchain>>,
        )> {
            if self.disabled_toolchains.contains(compiler_path) {
                bail!(
                    "Toolchain distribution for {:?} is disabled",
                    compiler_path.display()
                )
            }
            if let Some(tc_and_paths) = self.get_custom_toolchain(compiler_path).await {
                debug!("Using custom toolchain for {compiler_path:?}");
                let (tc, compiler_path, archive) = tc_and_paths?;
                return Ok((tc, Some((compiler_path, archive)), None));
            }

            // Only hash one toolchain at a time.
            // Not an issue if there are multiple attempts to
            // create the same toolchain, just a waste of time
            let mut cache = self.cache.lock().await;
            let mut weak_map = self.weak_map.lock().await;
            let mut pending_toolchains = self.pending_toolchains.lock().await;

            if let Some(archive_id) = weak_map.get(weak_toolchain_key) {
                let archive = cache.get_file(archive_id).ok();
                let package = pending_toolchains.get(archive_id).cloned();
                if archive.is_some() || package.is_some() {
                    if let Some(file) = archive {
                        trace!(
                            "Using cached toolchain {weak_toolchain_key:?} -> {archive_id:?} ({:?})",
                            file.path()
                        );
                    } else {
                        trace!("Using pending toolchain {weak_toolchain_key:?} -> {archive_id:?}");
                    }
                    return Ok((
                        Toolchain {
                            archive_id: archive_id.to_owned(),
                        },
                        None,
                        package,
                    ));
                }
            }

            debug!("Weak key appears to be new: {weak_toolchain_key:?}");

            let package = toolchain_packager
                .package(path_transformer)
                .await
                .context("Could not package toolchain")?;

            let archive_id = package
                .compute_hash()
                .await
                .context("Could not hash toolchain")?;

            pending_toolchains.insert(archive_id.clone(), package.clone());
            weak_map.insert(weak_toolchain_key.to_owned(), archive_id.clone());

            fs::File::create(self.cache_dir.join("weak_map.json"))
                .map_err(Error::from)
                .and_then(|f| serde_json::to_writer(f, &*weak_map).map_err(Error::from))
                .context("failed to enter toolchain in weak map")?;

            Ok((Toolchain { archive_id }, None, Some(package)))
        }

        // If the toolchain doesn't already exist, compress it and insert into the local cache
        pub async fn put_toolchain(
            &self,
            toolchain: &Toolchain,
            package: &dyn PackagedToolchain,
        ) -> Result<Option<fs::File>> {
            let archive_id = &toolchain.archive_id;
            // Only compress one toolchain at a time, because the default is to compress with all the cores.
            let mut cache = self.cache.lock().await;
            let mut pending_toolchains = self.pending_toolchains.lock().await;

            if let Ok(file) = cache.get_file(archive_id) {
                trace!("Using cached toolchain {archive_id:?} ({:?})", file.path());
                pending_toolchains.remove(archive_id);
                return Ok(Some(file));
            }

            let mut f = cache
                .prepare_add(archive_id, 0)
                .await
                .context("Failed to prepare toolchain cache dir")?;

            package
                .write_tar_gz(toolchain, f.as_file_mut())
                .await
                .context("Could not compress toolchain")?;

            cache.commit(f).await.context("Failed to cache toolchain")?;

            pending_toolchains.remove(archive_id);

            match cache.get_file(archive_id) {
                Ok(file) => Ok(Some(file)),
                Err(LruError::FileNotInCache) => Ok(None),
                Err(e) => Err(e).context("error while retrieving toolchain from cache"),
            }
        }

        pub async fn get_custom_toolchain(
            &self,
            compiler_path: &Path,
        ) -> Option<Result<(Toolchain, String, PathBuf)>> {
            match self
                .custom_toolchain_paths
                .lock()
                .await
                .get_mut(compiler_path)
            {
                Some((custom_tc, Some(tc))) => Some(Ok((
                    tc.clone(),
                    custom_tc.compiler_executable.clone(),
                    custom_tc.archive.clone(),
                ))),
                Some((custom_tc, maybe_tc @ None)) => {
                    let archive_id = match path_key(&custom_tc.archive).await {
                        Ok(archive_id) => archive_id,
                        Err(e) => return Some(Err(e)),
                    };
                    let tc = Toolchain { archive_id };
                    *maybe_tc = Some(tc.clone());
                    // If this entry already exists, someone has two custom toolchains with the same strong hash
                    if let Some(old_path) = self
                        .custom_toolchain_archives
                        .lock()
                        .await
                        .insert(tc.clone(), custom_tc.archive.clone())
                    {
                        // Log a warning if the user has identical toolchains at two different locations - it's
                        // not strictly wrong, but it is a bit odd
                        if old_path != custom_tc.archive {
                            warn!(
                                "Detected interchangeable toolchain archives at {} and {}",
                                old_path.display(),
                                custom_tc.archive.display()
                            );
                        }
                    }
                    Some(Ok((
                        tc,
                        custom_tc.compiler_executable.clone(),
                        custom_tc.archive.clone(),
                    )))
                }
                None => None,
            }
        }
    }

    #[cfg(all(
        test,
        feature = "dist-client",
        any(
            all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "aarch64")
            ),
            target_os = "freebsd"
        )
    ))]
    mod test_dist {
        use crate::{config, dist::PathTransformer, errors::*, test::utils::create_file};
        use std::{io::Write, sync::Arc};

        use {
            crate::dist::pkg::{PackagedToolchain, ToolchainPackager},
            async_trait::async_trait,
        };

        use super::ClientToolchains;

        struct PanicToolchainPackager;

        #[async_trait]
        impl ToolchainPackager for PanicToolchainPackager {
            async fn package(
                self: Box<Self>,
                _: &mut PathTransformer,
            ) -> Result<Arc<dyn PackagedToolchain>> {
                panic!("should not have called packager")
            }
        }

        #[tokio::test]
        async fn test_client_toolchains_custom() {
            let td = crate::util::temp_dir().unwrap();

            let ct1 =
                create_file(td.path(), "ct1", |mut f| f.write_all(b"toolchain_contents")).unwrap();

            let client_toolchains = ClientToolchains::new(
                &td.path().join("cache"),
                1024,
                &[config::DistToolchainConfig::PathOverride {
                    compiler_executable: "/my/compiler".into(),
                    archive: ct1.clone(),
                    archive_compiler_executable: "/my/compiler/in_archive".into(),
                }],
            )
            .unwrap();

            let (_, newpath, _) = client_toolchains
                .hash_toolchain(
                    "/my/compiler".as_ref(),
                    "weak_key",
                    Box::new(PanicToolchainPackager),
                    &mut PathTransformer::new(),
                )
                .await
                .unwrap();
            assert!(newpath.unwrap() == ("/my/compiler/in_archive".to_string(), ct1));
        }

        #[tokio::test]
        async fn test_client_toolchains_custom_multiuse_archive() {
            let td = crate::util::temp_dir().unwrap();

            let ct1 =
                create_file(td.path(), "ct1", |mut f| f.write_all(b"toolchain_contents")).unwrap();

            let client_toolchains = ClientToolchains::new(
                &td.path().join("cache"),
                1024,
                &[
                    config::DistToolchainConfig::PathOverride {
                        compiler_executable: "/my/compiler".into(),
                        archive: ct1.clone(),
                        archive_compiler_executable: "/my/compiler/in_archive".into(),
                    },
                    // Uses the same archive, but a maps a different external compiler to a different archive compiler
                    config::DistToolchainConfig::PathOverride {
                        compiler_executable: "/my/compiler2".into(),
                        archive: ct1.clone(),
                        archive_compiler_executable: "/my/compiler2/in_archive".into(),
                    },
                    // Uses the same archive, but a maps a different external compiler to the same archive compiler as the first
                    config::DistToolchainConfig::PathOverride {
                        compiler_executable: "/my/compiler3".into(),
                        archive: ct1.clone(),
                        archive_compiler_executable: "/my/compiler/in_archive".into(),
                    },
                ],
            )
            .unwrap();

            let (_, newpath, _) = client_toolchains
                .hash_toolchain(
                    "/my/compiler".as_ref(),
                    "weak_key",
                    Box::new(PanicToolchainPackager),
                    &mut PathTransformer::new(),
                )
                .await
                .unwrap();
            assert!(newpath.unwrap() == ("/my/compiler/in_archive".to_string(), ct1.clone()));
            let (_, newpath, _) = client_toolchains
                .hash_toolchain(
                    "/my/compiler2".as_ref(),
                    "weak_key",
                    Box::new(PanicToolchainPackager),
                    &mut PathTransformer::new(),
                )
                .await
                .unwrap();
            assert!(newpath.unwrap() == ("/my/compiler2/in_archive".to_string(), ct1.clone()));
            let (_, newpath, _) = client_toolchains
                .hash_toolchain(
                    "/my/compiler3".as_ref(),
                    "weak_key",
                    Box::new(PanicToolchainPackager),
                    &mut PathTransformer::new(),
                )
                .await
                .unwrap();
            assert!(newpath.unwrap() == ("/my/compiler/in_archive".to_string(), ct1));
        }

        #[tokio::test]
        async fn test_client_toolchains_nodist() {
            let td = crate::util::temp_dir().unwrap();

            let client_toolchains = ClientToolchains::new(
                &td.path().join("cache"),
                1024,
                &[config::DistToolchainConfig::NoDist {
                    compiler_executable: "/my/compiler".into(),
                }],
            )
            .unwrap();

            assert!(
                client_toolchains
                    .hash_toolchain(
                        "/my/compiler".as_ref(),
                        "weak_key",
                        Box::new(PanicToolchainPackager),
                        &mut PathTransformer::new(),
                    )
                    .await
                    .is_err()
            );
        }
    }

    #[cfg(test)]
    mod test_nodist {
        use crate::{config, test::utils::create_file};
        use std::io::Write;

        use super::ClientToolchains;

        #[test]
        fn test_client_toolchains_custom_nodist_conflict() {
            let td = crate::util::temp_dir().unwrap();

            let ct1 =
                create_file(td.path(), "ct1", |mut f| f.write_all(b"toolchain_contents")).unwrap();

            let client_toolchains = ClientToolchains::new(
                &td.path().join("cache"),
                1024,
                &[
                    config::DistToolchainConfig::PathOverride {
                        compiler_executable: "/my/compiler".into(),
                        archive: ct1,
                        archive_compiler_executable: "/my/compiler".into(),
                    },
                    config::DistToolchainConfig::NoDist {
                        compiler_executable: "/my/compiler".into(),
                    },
                ],
            );
            assert!(client_toolchains.is_err());
        }
    }
}

#[cfg(feature = "dist-server")]
mod server {
    use async_trait::async_trait;
    use flate2::bufread::MultiGzDecoder as GzipDecoder;
    use fs_err as fs;

    use std::{
        ffi::OsStr,
        io::BufReader,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use crate::cache::disk::DiskCache;
    use crate::cache::{Cache, Storage, cache};
    use crate::dist::metrics::{Metrics, TimeRecorder};
    use crate::dist::{Toolchain, ToolchainService};
    use crate::errors::*;

    const TC_LOAD: &str = "sccache::server::toolchain::load_time";
    const TC_LOAD_INFLATED: &str = "sccache::server::toolchain::load_inflated_time";
    const TC_LOAD_DEFLATED: &str = "sccache::server::toolchain::load_deflated_time";
    const TC_LOAD_INFLATED_SIZE: &str = "sccache::server::toolchain::load_inflated_size_time";
    const TC_UNPACK_INFLATED: &str = "sccache::server::toolchain::unpack_inflated_time";

    #[derive(Clone, Default)]
    pub struct ServerToolchainsMetrics {
        metrics: Metrics,
    }

    impl ServerToolchainsMetrics {
        pub fn new(metrics: Metrics) -> Self {
            metrics::describe_histogram!(
                TC_LOAD,
                metrics::Unit::Seconds,
                "The time to load a toolchain"
            );
            metrics::describe_histogram!(
                TC_LOAD_INFLATED,
                metrics::Unit::Seconds,
                "The time to load, inflate, and unpack a toolchain"
            );
            metrics::describe_histogram!(
                TC_LOAD_DEFLATED,
                metrics::Unit::Seconds,
                "The time to load a deflated toolchain"
            );
            metrics::describe_histogram!(
                TC_LOAD_INFLATED_SIZE,
                metrics::Unit::Seconds,
                "The time to calculate the inflated size of a toolchain"
            );
            metrics::describe_histogram!(
                TC_UNPACK_INFLATED,
                metrics::Unit::Seconds,
                "The time to inflate and unpack a toolchain"
            );
            Self { metrics }
        }

        pub fn load_timer(&self) -> TimeRecorder {
            self.metrics.timer(TC_LOAD)
        }

        pub fn load_inflated_timer(&self) -> TimeRecorder {
            self.metrics.timer(TC_LOAD_INFLATED)
        }

        pub fn load_deflated_timer(&self) -> TimeRecorder {
            self.metrics.timer(TC_LOAD_DEFLATED)
        }

        pub fn unpack_inflated_timer(&self) -> TimeRecorder {
            self.metrics.timer(TC_UNPACK_INFLATED)
        }
    }

    #[derive(Clone)]
    pub struct ServerToolchains {
        cache: Arc<DiskCache>,
        store: Arc<dyn cache::Storage>,
        metrics: ServerToolchainsMetrics,
        should_inflate_toolchains: bool,
    }

    #[async_trait]
    impl ToolchainService for ServerToolchains {
        async fn load_toolchain(&self, tc: &Toolchain) -> Result<PathBuf> {
            self.load(tc).await
        }
    }

    fn is_special_tokio_shutdown_io_error(err: &anyhow::Error) -> bool {
        if let Some(io_error) = err.downcast_ref::<std::io::Error>() {
            // tokio::fs returns an io::ErrorKind::Other error when a future
            // representing an IO operation is cancelled. This usually only
            // happens during server shutdown, so don't warn about this error
            // and bail early.
            if io_error.kind() == std::io::ErrorKind::Other {
                return true;
            }
        }
        false
    }

    impl ServerToolchains {
        pub fn new<P: AsRef<OsStr>>(
            root: P,
            max_size: u64,
            store: Arc<dyn cache::Storage>,
            metrics: Metrics,
            should_inflate_toolchains: bool,
        ) -> Self {
            Self {
                cache: Arc::new(DiskCache::new(
                    root,
                    max_size,
                    crate::cache::CacheMode::ReadWrite,
                    vec![],
                )),
                store,
                metrics: ServerToolchainsMetrics::new(metrics),
                should_inflate_toolchains,
            }
        }

        async fn load(&self, tc: &Toolchain) -> Result<PathBuf> {
            // Record toolchain load time after retrying
            let _timer = self.metrics.load_timer();
            // Load and cache the deflated toolchain.
            // Inflate, unpack, and cache it in a directory.
            // Return the path to the unpacked toolchain dir.
            self.load_toolchain(tc).await.map_err(|err| {
                if !is_special_tokio_shutdown_io_error(&err) {
                    tracing::error!(
                        "[ServerToolchains({})]: Error loading toolchain: {err:?}",
                        &tc.archive_id
                    );
                }
                err
            })
        }

        async fn load_toolchain(&self, tc: &Toolchain) -> Result<PathBuf> {
            // Record toolchain load_inflated time
            let _timer = self.metrics.load_inflated_timer();
            if let Ok((inflated_path, _)) = self.cache.entry(&tc.archive_id).await {
                // Return early if the toolchain is already loaded and unpacked
                Ok(inflated_path)
            } else {
                // Load the compressed toolchain
                let (deflated_path, deflated_size) = self.load_deflated_toolchain(tc).await?;
                if !self.should_inflate_toolchains {
                    Ok(deflated_path)
                } else {
                    // Inflate and unpack the toolchain archive
                    let inflated_path = self
                        .unpack_inflated_toolchain(&deflated_path, deflated_size, &tc.archive_id)
                        .await?;
                    Ok(inflated_path)
                }
            }
        }

        async fn load_deflated_toolchain(&self, tc: &Toolchain) -> Result<(PathBuf, u64)> {
            // Record toolchain load_deflated time
            let _timer = self.metrics.load_deflated_timer();
            let deflated_key = format!("{}.tgz", tc.archive_id);
            if !self.cache.has(&deflated_key).await {
                let entry = match self.store.get(&tc.archive_id).await? {
                    Cache::Hit(reader) => reader,
                    Cache::Miss => return Err(anyhow!("Missing toolchain")),
                };
                self.cache.put(&deflated_key, entry).await?;
            }
            self.cache.entry(&deflated_key).await
        }

        async fn unpack_inflated_toolchain(
            &self,
            deflated_path: &Path,
            deflated_size: u64,
            inflated_key: &str,
        ) -> Result<PathBuf> {
            // Record toolchain unpack_inflated time
            let _timer = self.metrics.unpack_inflated_timer();
            self.cache
                .insert_with(inflated_key, deflated_size, |inflated_path: &Path| {
                    let deflated_path = deflated_path.to_owned();
                    let inflated_path = inflated_path.to_owned();
                    async move {
                        tokio::task::spawn_blocking(move || {
                            // Ensure the inflated dir exists first
                            fs::create_dir_all(&inflated_path)?;
                            let deflated_file = fs::File::open(&deflated_path)?;
                            // Unpack the tgz into the inflated dir
                            tar::Archive::new(GzipDecoder::new(BufReader::new(deflated_file)))
                                .unpack(&inflated_path)
                                .map(|_| deflated_size)
                        })
                        .await?
                    }
                })
                .await
                .map(|(path, _)| path)
        }
    }
}
