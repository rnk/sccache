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

//! A [`dist::Client`] that distributes compilations over the Bazel Remote
//! Execution v2 API.
//!
//! This is an alternative to [`crate::dist::http::Client`], which talks to an
//! sccache-dist scheduler. Both sit behind the same trait, so the compiler
//! frontends, the retry loop in [`crate::compiler`], toolchain packaging and
//! local fallback are all shared and unchanged.
//!
//! What makes this cheap to build is that sccache dist-compiles *preprocessed*
//! source: the remote action is `clang++ -x c++-cpp-output ... -c in.ii -o
//! out.o`, with no include paths and no header inputs. There is no dependency
//! scanner here because there is nothing to scan -- the input root is one file,
//! plus the compiler itself when it is being shipped rather than baked into the
//! worker image.

mod cas;
mod exec;
mod merkle;
mod paths;
pub mod proto;
#[cfg(test)]
mod tests;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};

use crate::{
    config::{self, ReapiToolchainMode},
    dist::{
        self, BuildResult, CompileCommand, NewJobResponse, OutputData, RunJobResponse,
        SchedulerStatus, SubmitToolchainResult, Toolchain, cache,
        pkg::{PackagedToolchain, ToolchainPackager},
    },
    errors::*,
    mock_command::ProcessOutput,
};

use cas::Cas;
use merkle::DirBuilder;
use proto::build::bazel::remote::execution::v2 as reapi;

/// Metadata every request carries: the instance name and, optionally, a bearer
/// token.
pub struct RpcContext {
    pub instance_name: String,
    auth: Option<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>,
}

impl RpcContext {
    pub fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        if let Some(auth) = &self.auth {
            request
                .metadata_mut()
                .insert(http::header::AUTHORIZATION.as_str(), auth.clone());
        }
        request
    }
}

/// Turn a transport-level gRPC failure into an sccache error.
pub fn rpc_error(status: tonic::Status, what: &str) -> Error {
    anyhow!("{what} failed: {} ({:?})", status.message(), status.code())
}

/// Turn an application-level `google.rpc.Status` into an sccache error.
pub fn status_error(status: &proto::google::rpc::Status, what: &str) -> Error {
    anyhow!("{what} failed: {} (code {})", status.message, status.code)
}

/// gRPC status codes that mean "try again", as opposed to "this action is
/// broken".
fn is_retryable(code: i32) -> bool {
    // google.rpc.Code
    const CANCELLED: i32 = 1;
    const DEADLINE_EXCEEDED: i32 = 4;
    const RESOURCE_EXHAUSTED: i32 = 8;
    const ABORTED: i32 = 10;
    const INTERNAL: i32 = 13;
    const UNAVAILABLE: i32 = 14;
    matches!(
        code,
        CANCELLED | DEADLINE_EXCEEDED | RESOURCE_EXHAUSTED | ABORTED | INTERNAL | UNAVAILABLE
    )
}

/// `FAILED_PRECONDITION` from `Execute` conventionally means the server could
/// not find one of the blobs the action referenced -- it was evicted from the
/// CAS between our upload and the execution.
fn is_missing_blobs(code: i32) -> bool {
    const FAILED_PRECONDITION: i32 = 9;
    code == FAILED_PRECONDITION
}

/// A fresh handle onto `file`, positioned at the start.
///
/// `ingest_archive` reads its archive twice, and `File::try_clone` shares the
/// underlying file description -- including the cursor -- so the second reader
/// would otherwise start at EOF and silently see an empty archive.
fn rewound(file: &std::fs::File) -> std::io::Result<std::fs::File> {
    use std::io::{Seek, SeekFrom};
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

/// The state we hold between `new_job` and `run_job`.
struct JobState {
    /// The action's inputs, without the toolchain, which is merged in at
    /// `run_job` time so a single upload of the compiler serves every job.
    tree: DirBuilder,
}

pub struct Client {
    rpc: Arc<RpcContext>,
    channel: Channel,
    cas: Cas,
    reapi_config: config::ReapiConfig,
    tc_cache: Arc<cache::ClientToolchains>,
    fallback_to_local_compile: bool,
    max_retries: f64,
    request_timeout: u32,
    rewrite_includes_only: bool,
    jobs: Mutex<HashMap<String, JobState>>,
    /// Input-root fragments for toolchains we have already uploaded, keyed by
    /// `Toolchain::archive_id`.
    toolchains: tokio::sync::Mutex<HashMap<String, Arc<DirBuilder>>>,
    /// A stand-in server id for `RunJobResponse`, which wants to name the
    /// machine that ran the build. REv2 does not tell us.
    server_id: String,
}

/// `grpc://` and `grpcs://` are the conventional REv2 spellings; tonic wants
/// `http://` and `https://`.
fn normalize_url(url: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("Remote execution URL {url:?} has no scheme"))?;
    let scheme = match scheme {
        "grpc" | "http" => "http",
        "grpcs" | "https" => "https",
        other => bail!(
            "Unsupported remote execution URL scheme {other:?} in {url:?}, \
             expected one of grpc, grpcs, http, https"
        ),
    };
    Ok(format!("{scheme}://{rest}"))
}

impl Client {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        reapi_config: config::ReapiConfig,
        cache_dir: &Path,
        cache_size: u64,
        toolchain_configs: &[config::DistToolchainConfig],
        auth_token: Option<String>,
        fallback_to_local_compile: bool,
        max_retries: f64,
        rewrite_includes_only: bool,
        net: &config::DistNetworking,
    ) -> Result<Self> {
        let url = reapi_config
            .url
            .clone()
            .context("No remote execution URL configured")?;
        let normalized = normalize_url(&url)?;

        let mut endpoint = Endpoint::from_shared(normalized.clone())
            .with_context(|| format!("Invalid remote execution URL {url:?}"))?
            .connect_timeout(Duration::from_secs(net.connect_timeout as u64))
            // Deliberately no `.timeout()`: it would apply to the whole
            // `Execute` call, and that call stays open for as long as the
            // compilation takes.
            .http2_keep_alive_interval(Duration::from_secs(net.keepalive.interval))
            .keep_alive_timeout(Duration::from_secs(net.keepalive.timeout))
            .keep_alive_while_idle(net.keepalive.enabled);

        if normalized.starts_with("https://") {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .context("Failed to configure TLS for remote execution")?;
        }

        let channel = endpoint
            .connect()
            .await
            .with_context(|| format!("Failed to connect to remote execution service at {url}"))?;

        let auth = auth_token
            .filter(|token| !token.is_empty())
            .map(|token| {
                tonic::metadata::MetadataValue::try_from(format!("Bearer {token}"))
                    .context("Remote execution auth token is not a valid HTTP header value")
            })
            .transpose()?;

        let rpc = Arc::new(RpcContext {
            instance_name: reapi_config.instance_name.clone(),
            auth,
        });

        let capabilities = Self::fetch_capabilities(&rpc, &channel).await?;
        let max_batch_bytes = capabilities
            .cache_capabilities
            .as_ref()
            .map(|c| c.max_batch_total_size_bytes)
            .unwrap_or(0);

        // Refuse to start rather than produce digests the server cannot
        // interpret. Only SHA-256 is implemented here.
        if let Some(cache_capabilities) = capabilities.cache_capabilities.as_ref()
            && !cache_capabilities.digest_functions.is_empty()
            && !cache_capabilities
                .digest_functions
                .contains(&(reapi::digest_function::Value::Sha256 as i32))
        {
            bail!(
                "Remote execution service at {url} does not support the SHA-256 digest function, \
                 which is the only one sccache implements"
            );
        }

        let tc_cache = Arc::new(
            cache::ClientToolchains::new(cache_dir, cache_size, toolchain_configs)
                .context("failed to initialise client toolchains")?,
        );

        Ok(Self {
            cas: Cas::new(rpc.clone(), channel.clone(), max_batch_bytes),
            rpc,
            channel,
            reapi_config,
            tc_cache,
            fallback_to_local_compile,
            max_retries,
            request_timeout: net.request_timeout,
            rewrite_includes_only,
            jobs: Default::default(),
            toolchains: Default::default(),
            server_id: url,
        })
    }

    async fn fetch_capabilities(
        rpc: &Arc<RpcContext>,
        channel: &Channel,
    ) -> Result<reapi::ServerCapabilities> {
        reapi::capabilities_client::CapabilitiesClient::new(channel.clone())
            .get_capabilities(rpc.request(reapi::GetCapabilitiesRequest {
                instance_name: rpc.instance_name.clone(),
            }))
            .await
            .map(|res| res.into_inner())
            .map_err(|e| rpc_error(e, "GetCapabilities"))
    }

    /// Read a tar archive into an input-root fragment and upload whatever the
    /// server is missing.
    ///
    /// The archive is read twice on purpose: once to digest every entry
    /// without holding it in memory, and once -- after the server has told us
    /// what it lacks -- to materialize only the blobs that actually need
    /// uploading. On a warm CAS the second pass moves no data at all.
    async fn ingest_archive<F, R>(&self, open: F) -> Result<DirBuilder>
    where
        F: Fn() -> Result<R> + Send + Sync + 'static,
        R: std::io::Read + Send + 'static,
    {
        let open = Arc::new(open);

        let (tree, digests) = {
            let open = open.clone();
            tokio::task::spawn_blocking(move || -> Result<_> {
                let mut tree = DirBuilder::default();
                let digests = merkle::scan_tar(open()?, &mut tree)?;
                Ok((tree, digests))
            })
            .await
            .context("Failed to scan archive")??
        };

        let missing = self.cas.missing(&digests).await?;
        if missing.is_empty() {
            return Ok(tree);
        }

        let wanted: std::collections::HashSet<String> =
            missing.iter().map(|d| d.hash.clone()).collect();
        let wanted_sizes: std::collections::HashSet<u64> =
            missing.iter().map(|d| d.size_bytes as u64).collect();
        let total: i64 = missing.iter().map(|d| d.size_bytes).sum();
        debug!(
            "Uploading {} blob(s), {total} bytes, to the remote execution CAS",
            missing.len()
        );

        // Materialize the missing blobs in bounded batches so a large
        // toolchain does not have to fit in memory all at once.
        let mut pending: Vec<(reapi::Digest, Vec<u8>)> = Vec::new();
        let mut pending_bytes = 0i64;
        const FLUSH_BYTES: i64 = 32 * 1024 * 1024;

        let (send, mut recv) = tokio::sync::mpsc::channel::<(reapi::Digest, Vec<u8>)>(4);
        let reader = tokio::task::spawn_blocking(move || -> Result<()> {
            merkle::read_blobs(open()?, &wanted, &wanted_sizes, |digest, data| {
                send.blocking_send((digest, data))
                    .map_err(|_| anyhow!("Blob upload task went away"))
            })
        });

        while let Some((digest, data)) = recv.recv().await {
            pending_bytes += digest.size_bytes;
            pending.push((digest, data));
            if pending_bytes >= FLUSH_BYTES {
                self.cas.upload_all(std::mem::take(&mut pending)).await?;
                pending_bytes = 0;
            }
        }
        reader.await.context("Failed to read archive blobs")??;

        if !pending.is_empty() {
            self.cas.upload_all(pending).await?;
        }

        Ok(tree)
    }

    /// The toolchain's contribution to the input root, uploading it the first
    /// time it is asked for.
    async fn toolchain_tree(
        &self,
        toolchain: &Toolchain,
        packaged: Option<Arc<dyn PackagedToolchain>>,
    ) -> Result<Arc<DirBuilder>> {
        let mut cached = self.toolchains.lock().await;
        if let Some(tree) = cached.get(&toolchain.archive_id) {
            return Ok(tree.clone());
        }

        // Reuse sccache's existing on-disk toolchain cache: `CToolchainPackager`
        // already collects the compiler and everything it needs, with
        // root-relative paths, which is exactly the shape of an input root.
        let archive = if let Some(packaged) = packaged {
            self.tc_cache
                .put_toolchain(toolchain, packaged.as_ref())
                .await
        } else {
            self.tc_cache.get_toolchain(toolchain).await
        }?
        .with_context(|| {
            format!(
                "Toolchain {} is not in the local cache",
                toolchain.archive_id
            )
        })?;

        let path = archive.path().to_owned();
        let tree = self
            .ingest_archive(move || {
                let file = fs_err::File::open(&path)?;
                Ok(flate2::read::GzDecoder::new(file))
            })
            .await
            .with_context(|| format!("Failed to upload toolchain {}", toolchain.archive_id))?;

        let tree = Arc::new(tree);
        cached.insert(toolchain.archive_id.clone(), tree.clone());
        Ok(tree)
    }

    fn take_job(&self, job_id: &str) -> Option<DirBuilder> {
        self.jobs
            .lock()
            .unwrap()
            .get(job_id)
            .map(|state| state.tree.clone())
    }

    /// Turn a completed `ActionResult` into the shape sccache writes to disk.
    async fn collect_outputs(
        &self,
        result: reapi::ActionResult,
        output_paths: &std::collections::BTreeMap<String, String>,
    ) -> Result<BuildResult> {
        let stdout = self
            .inline_or_fetch(result.stdout_raw, result.stdout_digest)
            .await?;
        let stderr = self
            .inline_or_fetch(result.stderr_raw, result.stderr_digest)
            .await?;

        let mut outputs = Vec::with_capacity(result.output_files.len());
        for file in result.output_files {
            // Map the worker's relative path back to the absolute dist path
            // the local side is expecting.
            let Some(dist_path) = output_paths.get(&file.path) else {
                warn!("Ignoring unexpected remote output {:?}", file.path);
                continue;
            };

            let data = if !file.contents.is_empty() {
                file.contents
            } else if let Some(digest) = &file.digest {
                self.cas
                    .download(digest)
                    .await
                    .with_context(|| format!("Failed to fetch remote output {:?}", file.path))?
            } else {
                bail!(
                    "Remote output {:?} has neither contents nor a digest",
                    file.path
                );
            };

            outputs.push((
                dist_path.clone(),
                OutputData::try_from_reader(&data[..])
                    .with_context(|| format!("Failed to compress remote output {:?}", file.path))?,
            ));
        }

        Ok(BuildResult {
            output: ProcessOutput::new(result.exit_code as i64, stdout, stderr),
            outputs,
        })
    }

    async fn inline_or_fetch(
        &self,
        raw: Vec<u8>,
        digest: Option<reapi::Digest>,
    ) -> Result<Vec<u8>> {
        if !raw.is_empty() {
            return Ok(raw);
        }
        match digest {
            Some(digest) if digest.size_bytes > 0 => self.cas.download(&digest).await,
            _ => Ok(Vec::new()),
        }
    }
}

#[async_trait]
impl dist::Client for Client {
    async fn new_job(&self, toolchain: Toolchain, inputs: std::fs::File) -> Result<NewJobResponse> {
        let tree = self
            .ingest_archive(move || Ok(flate2::read::ZlibDecoder::new(rewound(&inputs)?)))
            .await
            .context("Failed to upload compilation inputs")?;

        let has_toolchain = match self.reapi_config.toolchain {
            // The compiler is in the worker's image; there is nothing to send.
            ReapiToolchainMode::Image => true,
            ReapiToolchainMode::Inputs => self
                .toolchains
                .lock()
                .await
                .contains_key(&toolchain.archive_id),
        };

        let job_id = uuid::Uuid::new_v4().to_string();
        self.jobs
            .lock()
            .unwrap()
            .insert(job_id.clone(), JobState { tree });

        Ok(NewJobResponse {
            has_inputs: true,
            has_toolchain,
            job_id,
            timeout: self.reapi_config.action_timeout_secs as u32,
        })
    }

    async fn put_job(&self, job_id: &str, inputs: std::fs::File) -> Result<()> {
        // Re-upload after the server reported blobs missing.
        let tree = self
            .ingest_archive(move || Ok(flate2::read::ZlibDecoder::new(rewound(&inputs)?)))
            .await
            .context("Failed to re-upload compilation inputs")?;

        self.jobs
            .lock()
            .unwrap()
            .insert(job_id.to_owned(), JobState { tree });
        Ok(())
    }

    async fn del_job(&self, job_id: &str) -> Result<()> {
        self.jobs.lock().unwrap().remove(job_id);
        Ok(())
    }

    async fn run_job(
        &self,
        job_id: &str,
        timeout: Duration,
        toolchain: Toolchain,
        command: CompileCommand,
        outputs: Vec<String>,
    ) -> Result<RunJobResponse> {
        let Some(mut tree) = self.take_job(job_id) else {
            return Ok(RunJobResponse::MissingJobInputs {
                server_id: self.server_id.clone(),
            });
        };

        if self.reapi_config.toolchain == ReapiToolchainMode::Inputs {
            let toolchain_tree = self
                .toolchains
                .lock()
                .await
                .get(&toolchain.archive_id)
                .cloned();
            let Some(toolchain_tree) = toolchain_tree else {
                return Ok(RunJobResponse::MissingToolchain {
                    server_id: self.server_id.clone(),
                });
            };
            tree.merge((*toolchain_tree).clone());
        }

        // REv2 requires the working directory to be "a directory which exists
        // in the input tree". Nothing else puts it there: the packagers only
        // tar up the preprocessed source and the directories leading to it,
        // and for an out-of-tree build the compile happens somewhere else
        // entirely. (Directories leading to the *outputs* are the worker's
        // job, per the spec, so they are deliberately not added here.)
        tree.insert_dir(paths::strip_root(&command.cwd))?;

        // Encoding the merged tree means re-serializing and re-hashing every
        // Directory in the toolchain, which is milliseconds for a real
        // toolchain but grows with its file count. Keep it off the runtime.
        let (tree, input_root, directory_blobs) = tokio::task::spawn_blocking(move || {
            let (root, blobs) = tree.finish();
            (tree, root, blobs)
        })
        .await
        .context("Failed to encode the action input root")?;

        // `Directory` messages are blobs too, and the server needs every one
        // of them to be able to walk the input root.
        let dir_digests: Vec<reapi::Digest> =
            directory_blobs.iter().map(|(d, _)| d.clone()).collect();
        let missing: std::collections::HashSet<String> = self
            .cas
            .missing(&dir_digests)
            .await?
            .into_iter()
            .map(|d| d.hash)
            .collect();
        self.cas
            .upload_all(
                directory_blobs
                    .into_iter()
                    .filter(|(d, _)| missing.contains(&d.hash))
                    .collect(),
            )
            .await
            .context("Failed to upload input root")?;

        // A failure here means the action could not be described at all --
        // in practice, `toolchain = "inputs"` with a compiler that was never
        // packaged. That is a permanent misconfiguration, so report it as
        // fatal; returning `Err` would let the retry loop spin on it, forever
        // when `max_retries` is `inf`.
        let plan = match exec::plan(
            &command,
            &outputs,
            &input_root,
            &tree,
            self.reapi_config.toolchain,
            &self.reapi_config.platform,
            &self.reapi_config.env_passthrough,
            timeout,
            self.reapi_config.do_not_cache,
        ) {
            Ok(plan) => plan,
            Err(err) => {
                return Ok(RunJobResponse::FatalError {
                    message: format!("{err:#}"),
                    server_id: self.server_id.clone(),
                });
            }
        };

        self.cas
            .upload_all(plan.blobs.clone())
            .await
            .context("Failed to upload action")?;

        debug!(
            "[{job_id}]: executing action {} ({})",
            plan.action_digest.hash, plan.command.arguments[0]
        );

        let response = match exec::execute(
            &self.rpc,
            &self.channel,
            &plan.action_digest,
            self.reapi_config.skip_cache_lookup,
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                return Ok(RunJobResponse::RetryableError {
                    message: format!("{err:#}"),
                    server_id: self.server_id.clone(),
                });
            }
        };

        // A nonzero compiler exit code is a *successful* action: the build
        // failed, not the remote execution. Only a non-OK status here means
        // the action itself could not be run.
        if let Some(status) = response.status.as_ref()
            && status.code != 0
        {
            let message = format!(
                "{} (code {}){}",
                status.message,
                status.code,
                if response.message.is_empty() {
                    String::new()
                } else {
                    format!(": {}", response.message)
                }
            );
            return Ok(if is_missing_blobs(status.code) {
                // The CAS lost something between our upload and execution;
                // sccache's retry loop will re-send the inputs. Drop what we
                // believe the server has first, or the resubmit would decide
                // everything is already present and upload nothing.
                self.cas.forget_known_present();
                debug!("[{job_id}]: remote execution reported missing blobs: {message}");
                RunJobResponse::MissingJobInputs {
                    server_id: self.server_id.clone(),
                }
            } else if is_retryable(status.code) {
                RunJobResponse::RetryableError {
                    message,
                    server_id: self.server_id.clone(),
                }
            } else {
                RunJobResponse::FatalError {
                    message,
                    server_id: self.server_id.clone(),
                }
            });
        }

        let Some(result) = response.result else {
            return Ok(RunJobResponse::MissingJobResult {
                server_id: self.server_id.clone(),
            });
        };

        if response.cached_result {
            trace!("[{job_id}]: remote action cache hit");
        }

        match self.collect_outputs(result, &plan.output_paths).await {
            Ok(result) => Ok(RunJobResponse::Complete {
                result,
                server_id: self.server_id.clone(),
            }),
            Err(err) => Ok(RunJobResponse::RetryableError {
                message: format!("{err:#}"),
                server_id: self.server_id.clone(),
            }),
        }
    }

    async fn get_status(&self) -> Result<SchedulerStatus> {
        // REv2 has no notion of "how many workers are there"; capabilities is
        // the closest thing to a health check the API offers, and it is what
        // startup uses to decide the endpoint is usable.
        let capabilities = Self::fetch_capabilities(&self.rpc, &self.channel).await?;
        if capabilities.execution_capabilities.is_none() {
            bail!(
                "Remote execution service at {} advertises no execution capabilities; \
                 it may be a cache-only endpoint",
                self.server_id
            );
        }
        Ok(SchedulerStatus {
            info: Default::default(),
            jobs: Default::default(),
            servers: vec![dist::ServerStatus {
                id: self.server_id.clone(),
                ..Default::default()
            }],
        })
    }

    async fn put_toolchain(
        &self,
        toolchain: Toolchain,
        packaged: Option<Arc<dyn PackagedToolchain>>,
    ) -> Result<SubmitToolchainResult> {
        if self.reapi_config.toolchain == ReapiToolchainMode::Image {
            return Ok(SubmitToolchainResult::Success);
        }
        match self.toolchain_tree(&toolchain, packaged).await {
            Ok(_) => Ok(SubmitToolchainResult::Success),
            Err(err) => Ok(SubmitToolchainResult::Error {
                message: format!("{err:#}"),
            }),
        }
    }

    async fn hash_toolchain(
        &self,
        compiler_path: &Path,
        weak_toolchain_key: &str,
        toolchain_packager: Box<dyn ToolchainPackager>,
        path_transformer: &mut dist::PathTransformer,
    ) -> Result<(
        Toolchain,
        Option<(String, PathBuf)>,
        Option<Arc<dyn PackagedToolchain>>,
    )> {
        if self.reapi_config.toolchain == ReapiToolchainMode::Image {
            // Packaging a toolchain means running the compiler several times
            // and tarring up its shared libraries. When the compiler is baked
            // into the worker's image, none of that work is ever used, so skip
            // it entirely and identify the toolchain by its weak key.
            return Ok((
                Toolchain {
                    archive_id: weak_toolchain_key.to_owned(),
                },
                None,
                None,
            ));
        }

        self.tc_cache
            .hash_toolchain(
                compiler_path,
                weak_toolchain_key,
                toolchain_packager,
                path_transformer,
            )
            .await
    }

    fn fallback_to_local_compile(&self) -> bool {
        self.fallback_to_local_compile
    }

    fn max_retries(&self) -> f64 {
        self.max_retries
    }

    fn request_timeout(&self) -> u32 {
        self.request_timeout
    }

    fn rewrite_includes_only(&self) -> bool {
        self.rewrite_includes_only
    }

    fn stage_sources(&self) -> bool {
        self.reapi_config.stage == config::ReapiStageMode::Sources
    }

    async fn get_custom_toolchain(&self, exe: &Path) -> Option<PathBuf> {
        match self.tc_cache.get_custom_toolchain(exe).await {
            Some(Ok((_, _, path))) => Some(path),
            _ => None,
        }
    }
}

#[cfg(test)]
mod url_test {
    use super::*;

    #[test]
    fn grpc_schemes_map_to_http() {
        assert_eq!(
            normalize_url("grpc://host:50051").unwrap(),
            "http://host:50051"
        );
        assert_eq!(
            normalize_url("grpcs://host:443").unwrap(),
            "https://host:443"
        );
        assert_eq!(normalize_url("http://host").unwrap(), "http://host");
        assert_eq!(normalize_url("https://host").unwrap(), "https://host");
        assert!(normalize_url("host:50051").is_err());
        assert!(normalize_url("ftp://host").is_err());
    }
}
