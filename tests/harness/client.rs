use fs_err as fs;
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str,
    sync::atomic::{AtomicU16, Ordering},
};

use sccache::{
    config::{
        CacheConfigs, DiskCacheConfig, DistConfig, DistNetworking, FileConfig,
        PreprocessorCacheModeConfig, try_read_config_file,
    },
    server::{ServerInfo, ServerStats},
};

use crate::harness::write_json_cfg;

use super::{TC_CACHE_SIZE, prune_command};

pub fn make_sccache_client(
    preprocessor_cache_mode: bool,
) -> (Option<tempfile::TempDir>, PathBuf, SccacheClient) {
    let tempdir = tempfile::Builder::new()
        .prefix("sccache_system_test")
        .tempdir()
        .unwrap();

    // Persist the tempdir if SCCACHE_DEBUG is defined
    let (tempdir_path, maybe_tempdir) = if env::var("SCCACHE_DEBUG").is_ok() {
        (tempdir.keep(), None)
    } else {
        (tempdir.path().to_path_buf(), Some(tempdir))
    };

    // Create the configurations
    let sccache_cfg = sccache_client_cfg(&tempdir_path, preprocessor_cache_mode);
    write_json_cfg(&tempdir_path, "sccache-cfg.json", &sccache_cfg);
    let sccache_cached_cfg_path = tempdir_path.join("sccache-cached-cfg");
    // Start the server daemon on a unique port
    let client = SccacheClient::new(
        &tempdir_path.join("sccache-cfg.json"),
        &sccache_cached_cfg_path,
    );

    (maybe_tempdir, tempdir_path, client)
}

pub fn sccache_client_cfg(data_dir: &Path, preprocessor_cache_mode: bool) -> FileConfig {
    let disk_cache_dir = data_dir.join("cache");
    let toolchains_dir = data_dir.join("toolchains");
    fs::create_dir_all(&disk_cache_dir).unwrap();
    fs::create_dir_all(&toolchains_dir).unwrap();

    let disk_cache = DiskCacheConfig {
        dir: disk_cache_dir,
        preprocessor_cache_mode: PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: preprocessor_cache_mode,
            ..Default::default()
        },
        ..Default::default()
    };
    FileConfig {
        cache: CacheConfigs {
            azure: None,
            disk: Some(disk_cache),
            gcs: None,
            gha: None,
            memcached: None,
            redis: None,
            s3: None,
            webdav: None,
            oss: None,
            cos: None,
            multilevel: None,
        },
        dist: DistConfig {
            auth: Default::default(), // dangerously_insecure
            scheduler_url: None,
            cache_dir: toolchains_dir,
            toolchains: vec![],
            toolchain_cache_size: TC_CACHE_SIZE,
            rewrite_includes_only: false, // TODO
            net: DistNetworking {
                max_connections: 1,
                connect_timeout: 10,
                request_timeout: 300,
                ..Default::default()
            },
            ..Default::default()
        },
        server_startup_timeout_ms: None,
        basedirs: vec![],
    }
}

static CLIENT_PORT: AtomicU16 = AtomicU16::new(4227);

pub struct SccacheClient {
    envvars: Vec<(OsString, OsString)>,
    pub path: PathBuf,
}

impl SccacheClient {
    pub fn new_no_cfg() -> Self {
        let path = env!("CARGO_BIN_EXE_sccache").into();
        let port = CLIENT_PORT.fetch_add(1, Ordering::SeqCst);

        let mut envvars = vec![
            ("SCCACHE_SERVER_PORT".into(), port.to_string().into()),
            ("SCCACHE_THREADS".into(), "2".into()),
        ];

        // Send daemon logs to a file if SCCACHE_DEBUG is defined
        if env::var("SCCACHE_DEBUG").is_ok() {
            envvars.extend_from_slice(&[
                // Allow overriding log level
                (
                    "SCCACHE_SERVER_LOG".into(),
                    env::var_os("SCCACHE_SERVER_LOG")
                        .or(env::var_os("SCCACHE_LOG"))
                        .unwrap_or("sccache=trace".into()),
                ),
                // Allow overriding log output path
                (
                    "SCCACHE_ERROR_LOG".into(),
                    env::var_os("SCCACHE_ERROR_LOG").unwrap_or(
                        env::temp_dir()
                            .join(format!("sccache_local_daemon.{port}.txt"))
                            .into_os_string(),
                    ),
                ),
            ]);
        }

        Self { envvars, path }
    }

    pub fn new(cfg_path: &Path, cached_cfg_path: &Path) -> Self {
        let mut this = Self::new_no_cfg();
        this.envvars.push(("SCCACHE_CONF".into(), cfg_path.into()));
        this.envvars
            .push(("SCCACHE_CACHED_CONF".into(), cached_cfg_path.into()));
        this
    }

    pub fn start(self) -> Self {
        trace!("sccache --start-server");
        // Don't run this with run() because on Windows `wait_with_output`
        // will hang because the internal server process is not detached.
        if !self
            .cmd()
            .arg("--start-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            panic!("Failed to start local daemon");
        }
        self
    }

    pub fn stop(&self) -> bool {
        trace!("sccache --stop-server");
        self.cmd()
            .arg("--stop-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    pub fn cmd(&self) -> Command {
        let mut cmd = prune_command(Command::new(self.path.as_os_str()));
        cmd.envs(
            self.envvars
                .iter()
                .map(|(k, v)| (k.as_os_str(), v.as_os_str())),
        );
        cmd
    }

    pub fn info(&self) -> sccache::errors::Result<ServerInfo> {
        self.cmd()
            .args(["--show-stats", "--stats-format=json"])
            .output()
            .map_err(anyhow::Error::new)
            .map_err(|e| e.context("`sccache --show-stats --stats-format=json` failed"))
            .map(|output| {
                let s = str::from_utf8(&output.stdout).expect("Output not UTF-8");
                serde_json::from_str(s).expect("Failed to parse JSON stats")
            })
    }

    pub fn stats(&self) -> sccache::errors::Result<ServerStats> {
        self.info().map(|info| info.stats)
    }

    pub fn zero_stats(&self) {
        trace!("sccache --zero-stats");
        drop(
            self.cmd()
                .arg("--zero-stats")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        );
    }

    pub fn config(&self) -> FileConfig {
        if let Some(cfg_path_idx) = self.envvars.iter().position(|(k, _)| k == "SCCACHE_CONF")
            && let Some((_, cfg_path)) = self.envvars.get(cfg_path_idx)
            && let Ok(Some(cfg)) = try_read_config_file::<FileConfig>(&PathBuf::from(cfg_path))
        {
            return cfg;
        }
        FileConfig::default()
    }

    pub fn disk_cache_config(&self) -> DiskCacheConfig {
        self.config().cache.disk.unwrap_or_default()
    }

    pub fn clear_disk_cache(&self) -> sccache::errors::Result<(PathBuf, PathBuf)> {
        Ok((self.clear_object_cache()?, self.clear_preprocessor_cache()?))
    }

    pub fn clear_preprocessor_cache(&self) -> sccache::errors::Result<PathBuf> {
        let disk_cache = self.disk_cache_config();
        let preprocessor_cache_dir = disk_cache
            .dir
            .join(disk_cache.preprocessor_cache_mode.key_prefix);
        println!("clear_preprocessor_cache: {preprocessor_cache_dir:?}");
        if preprocessor_cache_dir.is_dir() {
            fs::remove_dir_all(&preprocessor_cache_dir)?;
        }
        Ok(preprocessor_cache_dir)
    }

    pub fn clear_object_cache(&self) -> sccache::errors::Result<PathBuf> {
        let disk_cache = self.disk_cache_config();
        let object_cache_dir = disk_cache.dir;
        println!("clear_object_cache: {object_cache_dir:?}");
        for path in "0123456789abcdef"
            .chars()
            .map(|c| object_cache_dir.join(String::from(c)))
        {
            if path.is_dir() {
                println!("rmdir: {path:?}");
                fs::remove_dir_all(path)?;
            }
        }
        Ok(object_cache_dir)
    }

    pub fn clear_toolchains_cache(&self) -> sccache::errors::Result<PathBuf> {
        let dist_config = self.config().dist;
        let tc_dir = dist_config.cache_dir.join("client").join("tc");
        println!("clear_toolchains_cache: {tc_dir:?}");
        if tc_dir.exists() {
            for entry in std::fs::read_dir(&tc_dir)? {
                let path = entry?.path();
                if fs::symlink_metadata(&path)?.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
        }
        Ok(tc_dir.clone())
    }
}

impl Drop for SccacheClient {
    fn drop(&mut self) {
        self.stop();
    }
}
