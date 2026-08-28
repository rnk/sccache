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
mod digest_cache;
mod exec;
mod merkle;
mod paths;
pub mod proto;
#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, HashMap},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint};

use crate::{
    config::{self, ReapiToolchainMode},
    dist::{
        self, AllocJobResult, BuildResult, CompileCommand, JobAlloc, JobComplete, JobId,
        OutputData, PathTransformer, ProcessOutput, RunJobResult, SchedulerStatusResult, ServerId,
        SubmitToolchainResult, Toolchain, cache,
        pkg::{self, InputEntry, ToolchainPackager},
    },
    errors::*,
};

use cas::Cas;
use digest_cache::DigestCache;
use merkle::DirBuilder;
use proto::build::bazel::remote::execution::v2 as reapi;

/// Metadata every request carries: the instance name, and whatever headers the
/// deployment needs -- typically an `authorization` header, plus any routing
/// headers the service expects.
pub struct RpcContext {
    pub instance_name: String,
    headers: Vec<(
        tonic::metadata::MetadataKey<tonic::metadata::Ascii>,
        tonic::metadata::MetadataValue<tonic::metadata::Ascii>,
    )>,
}

impl RpcContext {
    pub fn request<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        for (name, value) in self.headers.iter() {
            request.metadata_mut().insert(name.clone(), value.clone());
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

/// Where the bytes of a blob we may need to upload can be found.
enum BlobSource {
    File(PathBuf),
    Memory(Vec<u8>),
}

/// Input-root paths are `String` in the REv2 wire format; the packagers hand
/// them over as `PathBuf`.
fn path_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .with_context(|| format!("Input path {path:?} is not valid UTF-8"))
}

pub struct Client {
    rpc: Arc<RpcContext>,
    channel: Channel,
    cas: Cas,
    reapi_config: config::ReapiConfig,
    tc_cache: Arc<cache::ClientToolchains>,
    pool: tokio::runtime::Handle,
    rewrite_includes_only: bool,
    /// Content hashes for files we have already hashed, so a header shared by
    /// a thousand translation units is read once rather than a thousand times.
    digests: Arc<DigestCache>,
    /// Input-root fragments for toolchains we have already uploaded, keyed by
    /// `Toolchain::archive_id`.
    toolchains: tokio::sync::Mutex<HashMap<String, Arc<DirBuilder>>>,
    /// `JobId` is sccache's handle on a job, and nothing outside this client
    /// ever interprets it, so a local counter is enough.
    next_job_id: AtomicU64,
    /// `ServerId` wants the address of the machine that ran the build. REv2
    /// does not tell us, so this names the endpoint instead.
    server_addr: SocketAddr,
    /// The configured URL, for error messages.
    server_name: String,
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

/// A `SocketAddr` to put in `ServerId`, which is only ever displayed.
///
/// REv2 does not tell us which worker ran an action, and the endpoint may not
/// even be an IP literal, so resolve what we can and fall back to a
/// placeholder rather than failing a build over a diagnostic.
fn endpoint_addr(url: &str) -> SocketAddr {
    let default = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    let Some((_, rest)) = url.split_once("://") else {
        return default;
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    use std::net::ToSocketAddrs;
    authority
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .unwrap_or(default)
}

/// How long to wait for the initial connection to the remote execution
/// service. Distinct from the action timeout, which is per-compilation and
/// configurable; this one only covers getting a channel up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// HTTP/2 keepalive. A compile action can hold a stream open for minutes with
/// no traffic, and intermediaries will drop an idle connection without it.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

impl Client {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        reapi_config: config::ReapiConfig,
        cache_dir: &Path,
        cache_size: u64,
        toolchain_configs: &[config::DistToolchainConfig],
        auth_token: Option<String>,
        rewrite_includes_only: bool,
        pool: &tokio::runtime::Handle,
    ) -> Result<Self> {
        let url = reapi_config
            .url
            .clone()
            .context("No remote execution URL configured")?;
        let normalized = normalize_url(&url)?;

        let mut endpoint = Endpoint::from_shared(normalized.clone())
            .with_context(|| format!("Invalid remote execution URL {url:?}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            // Deliberately no `.timeout()`: it would apply to the whole
            // `Execute` call, and that call stays open for as long as the
            // compilation takes.
            .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
            .keep_alive_timeout(KEEPALIVE_TIMEOUT)
            .keep_alive_while_idle(true);

        if normalized.starts_with("https://") {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .context("Failed to configure TLS for remote execution")?;
        }

        let channel = endpoint
            .connect()
            .await
            .with_context(|| format!("Failed to connect to remote execution service at {url}"))?;

        // The token, when present, is the conventional bearer credential.
        // Anything in `headers` wins, so a deployment behind a proxy that
        // wants a different scheme can spell out the whole header itself.
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        if let Some(token) = auth_token.filter(|token| !token.is_empty()) {
            headers.insert(
                http::header::AUTHORIZATION.as_str().to_owned(),
                format!("Bearer {token}"),
            );
        }
        headers.extend(
            reapi_config
                .headers
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
        let headers = headers
            .into_iter()
            .map(|(name, value)| -> Result<_> {
                let key = tonic::metadata::MetadataKey::from_bytes(name.as_bytes())
                    .with_context(|| format!("{name:?} is not a valid header name"))?;
                let value = tonic::metadata::MetadataValue::try_from(&value)
                    .with_context(|| format!("The value of header {name:?} is not valid ASCII"))?;
                Ok((key, value))
            })
            .collect::<Result<Vec<_>>>()?;

        let rpc = Arc::new(RpcContext {
            instance_name: reapi_config.instance_name.clone(),
            headers,
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
            digests: DigestCache::new(),
            rpc,
            channel,
            reapi_config,
            tc_cache,
            pool: pool.clone(),
            rewrite_includes_only,
            toolchains: Default::default(),
            next_job_id: AtomicU64::new(1),
            server_addr: endpoint_addr(&normalized),
            server_name: url,
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

    /// Build an input-root fragment from individually named inputs, uploading
    /// whatever the server is missing.
    ///
    /// This is [`Self::ingest_archive`] without the archive, and it is the
    /// better shape for REv2 in every respect. The archive path has to copy
    /// every input into a tar, then decompress and parse that tar twice --
    /// once to hash the entries and once to pull out the blobs the server
    /// wants -- for content that is already sitting on the local disk. Here
    /// the files are hashed where they lie, through [`DigestCache`], so the
    /// second and later jobs that reference a header pay a `stat` instead of
    /// a read; and the only bytes that ever get loaded into memory are the
    /// ones actually being uploaded.
    async fn ingest_entries(&self, entries: Arc<Vec<InputEntry>>) -> Result<DirBuilder> {
        let digests = self.digests.clone();
        let scan = entries.clone();

        // `uploadable` maps digest -> where to get those bytes, for the
        // subset the server turns out to be missing.
        let (tree, all_digests, uploadable) = tokio::task::spawn_blocking(
            move || -> Result<(DirBuilder, Vec<reapi::Digest>, HashMap<String, BlobSource>)> {
                let mut tree = DirBuilder::default();
                let mut all = Vec::new();
                let mut uploadable = HashMap::new();

                for entry in scan.iter() {
                    match entry {
                        InputEntry::Dir { dist_path } => {
                            tree.insert_dir(&path_str(dist_path)?)?;
                        }
                        InputEntry::Symlink { dist_path, target } => {
                            tree.insert_symlink(&path_str(dist_path)?, &path_str(target)?)?;
                        }
                        InputEntry::File {
                            dist_path,
                            src_path,
                        } => {
                            let found = digests.digest_of(src_path)?;
                            tree.insert_file(
                                &path_str(dist_path)?,
                                found.digest.clone(),
                                found.is_executable,
                            )?;
                            all.push(found.digest.clone());
                            uploadable.insert(
                                found.digest.hash.clone(),
                                BlobSource::File(src_path.clone()),
                            );
                        }
                        InputEntry::Blob { dist_path, data } => {
                            let digest = merkle::digest_bytes(data);
                            tree.insert_file(&path_str(dist_path)?, digest.clone(), false)?;
                            all.push(digest.clone());
                            uploadable
                                .insert(digest.hash.clone(), BlobSource::Memory(data.clone()));
                        }
                    }
                }

                Ok((tree, all, uploadable))
            },
        )
        .await
        .context("Failed to build the action input root")??;

        let missing = self.cas.missing(&all_digests).await?;
        if missing.is_empty() {
            return Ok(tree);
        }

        let total: i64 = missing.iter().map(|d| d.size_bytes).sum();
        debug!(
            "Uploading {} blob(s), {total} bytes, to the remote execution CAS",
            missing.len()
        );

        // Load and ship the missing blobs in bounded batches, so a job with a
        // very large cold input set never has all of it in memory at once.
        const FLUSH_BYTES: i64 = 32 * 1024 * 1024;
        let mut pending: Vec<(reapi::Digest, Vec<u8>)> = Vec::new();
        let mut pending_bytes = 0i64;

        for digest in missing.iter() {
            let Some(source) = uploadable.get(&digest.hash) else {
                // The server asked for something we did not offer it.
                bail!(
                    "Remote execution service reported a missing blob {} that is not \
                     part of this job",
                    digest.hash
                );
            };
            let data = match source {
                BlobSource::Memory(data) => data.clone(),
                BlobSource::File(path) => {
                    let digests = self.digests.clone();
                    let path = path.clone();
                    let digest = digest.clone();
                    tokio::task::spawn_blocking(move || digests.read_for_upload(&path, &digest))
                        .await
                        .context("Failed to read an input for upload")??
                }
            };

            pending_bytes += digest.size_bytes;
            pending.push((digest.clone(), data));
            if pending_bytes >= FLUSH_BYTES {
                self.cas.upload_all(std::mem::take(&mut pending)).await?;
                pending_bytes = 0;
            }
        }

        if !pending.is_empty() {
            self.cas.upload_all(pending).await?;
        }

        Ok(tree)
    }

    /// Build an input-root fragment from whichever form the inputs arrived in.
    /// The toolchain's contribution to the input root, uploading it the first
    /// time it is asked for.
    async fn toolchain_tree(&self, toolchain: &Toolchain) -> Result<Arc<DirBuilder>> {
        let mut cached = self.toolchains.lock().await;
        if let Some(tree) = cached.get(&toolchain.archive_id) {
            return Ok(tree.clone());
        }

        // Reuse sccache's existing on-disk toolchain cache: `CToolchainPackager`
        // already collects the compiler and everything it needs, with
        // root-relative paths, which is exactly the shape of an input root.
        let file = self.tc_cache.get_toolchain(toolchain)?.with_context(|| {
            format!(
                "Toolchain {} is not in the local cache",
                toolchain.archive_id
            )
        })?;

        let tree = self
            .ingest_archive(move || Ok(flate2::read::GzDecoder::new(rewound(file.file())?)))
            .await
            .with_context(|| format!("Failed to upload toolchain {}", toolchain.archive_id))?;

        let tree = Arc::new(tree);
        cached.insert(toolchain.archive_id.clone(), tree.clone());
        Ok(tree)
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
            output: ProcessOutput {
                code: result.exit_code,
                stdout,
                stderr,
            },
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
#[async_trait]
impl dist::Client for Client {
    /// REv2 has no scheduler and nothing to allocate, so this only answers
    /// the one question the caller actually needs: does the toolchain still
    /// have to be uploaded?
    async fn do_alloc_job(&self, tc: Toolchain) -> Result<AllocJobResult> {
        let need_toolchain = match self.reapi_config.toolchain {
            // The compiler is in the worker's image; there is nothing to send.
            ReapiToolchainMode::Image => false,
            ReapiToolchainMode::Inputs => {
                !self.toolchains.lock().await.contains_key(&tc.archive_id)
            }
        };
        Ok(AllocJobResult::Success {
            job_alloc: JobAlloc {
                // `auth` is an opaque string the scheduler hands out and hands
                // back untouched in `do_run_job`. There is no scheduler here,
                // so we use it to carry the one thing `do_run_job` needs and
                // is not otherwise given: which toolchain this job wants.
                auth: tc.archive_id,
                job_id: JobId(self.next_job_id.fetch_add(1, Ordering::Relaxed)),
                server_id: ServerId::new(self.server_addr),
            },
            need_toolchain,
        })
    }

    async fn do_get_status(&self) -> Result<SchedulerStatusResult> {
        // REv2 has no notion of "how many workers are there"; capabilities is
        // the closest thing to a health check the API offers, and it is what
        // startup uses to decide the endpoint is usable.
        let capabilities = Self::fetch_capabilities(&self.rpc, &self.channel).await?;
        if capabilities.execution_capabilities.is_none() {
            bail!(
                "Remote execution service at {} advertises no execution capabilities; \
                 it may be a cache-only endpoint",
                self.server_name
            );
        }
        Ok(SchedulerStatusResult {
            num_servers: 1,
            num_cpus: 0,
            in_progress: 0,
        })
    }

    async fn do_submit_toolchain(
        &self,
        _job_alloc: JobAlloc,
        tc: Toolchain,
    ) -> Result<SubmitToolchainResult> {
        if self.reapi_config.toolchain == ReapiToolchainMode::Image {
            return Ok(SubmitToolchainResult::Success);
        }
        match self.toolchain_tree(&tc).await {
            Ok(_) => Ok(SubmitToolchainResult::Success),
            Err(err) => {
                warn!("Failed to upload toolchain {}: {err:#}", tc.archive_id);
                Ok(SubmitToolchainResult::CannotCache)
            }
        }
    }

    async fn do_run_job(
        &self,
        job_alloc: JobAlloc,
        command: CompileCommand,
        outputs: Vec<String>,
        inputs_packager: Box<dyn pkg::InputsPackager>,
    ) -> Result<(RunJobResult, PathTransformer)> {
        let job_id = job_alloc.job_id;

        // Staging inputs individually is the whole reason this client is
        // worth having: the server content-addresses each file, so a header
        // shared by a thousand translation units is uploaded once and hashed
        // once. Falling back to an archive keeps packagers that cannot
        // enumerate their inputs (Rust, today) working.
        let (mut tree, path_transformer) = if inputs_packager.can_list_inputs() {
            let (entries, pt) = tokio::task::spawn_blocking(move || inputs_packager.list_inputs())
                .await
                .context("Failed to enumerate compilation inputs")??;
            (self.ingest_entries(Arc::new(entries)).await?, pt)
        } else {
            let (file, pt) = tokio::task::spawn_blocking(move || -> Result<_> {
                let mut archive = tempfile::tempfile()?;
                let pt = inputs_packager.write_inputs(&mut archive)?;
                Ok((archive, pt))
            })
            .await
            .context("Failed to package compilation inputs")??;
            let tree = self
                .ingest_archive(move || Ok(rewound(&file)?))
                .await
                .context("Failed to upload compilation inputs")?;
            (tree, pt)
        };

        if self.reapi_config.toolchain == ReapiToolchainMode::Inputs {
            // `do_submit_toolchain` has already uploaded this if it was
            // needed, so in the steady state this is a lookup.
            let toolchain_tree = self
                .toolchains
                .lock()
                .await
                .get(&job_alloc.auth)
                .cloned()
                .with_context(|| {
                    format!(
                        "Toolchain {} was never uploaded for job {job_id}",
                        job_alloc.auth
                    )
                })?;
            tree.merge((*toolchain_tree).clone());
        }

        // REv2 requires the working directory to be "a directory which exists
        // in the input tree". Nothing else puts it there: the packagers only
        // stage the source and the directories leading to it, and for an
        // out-of-tree build the compile happens somewhere else entirely.
        // (Directories leading to the *outputs* are the worker's job, per the
        // spec, so they are deliberately not added here.)
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

        let plan = exec::plan(
            &command,
            &outputs,
            &input_root,
            &tree,
            self.reapi_config.toolchain,
            &self.reapi_config.platform,
            &self.reapi_config.env_passthrough,
            Duration::from_secs(self.reapi_config.action_timeout_secs),
            self.reapi_config.do_not_cache,
        )
        .context("Failed to describe the remote action")?;

        self.cas
            .upload_all(plan.blobs.clone())
            .await
            .context("Failed to upload action")?;

        debug!(
            "[{job_id}]: executing action {} ({})",
            plan.action_digest.hash, plan.command.arguments[0]
        );

        let response = exec::execute(
            &self.rpc,
            &self.channel,
            &plan.action_digest,
            self.reapi_config.skip_cache_lookup,
        )
        .await
        .context("Remote execution failed")?;

        // A nonzero compiler exit code is a *successful* action: the build
        // failed, not the remote execution. Only a non-OK status here means
        // the action itself could not be run.
        if let Some(status) = response.status.as_ref()
            && status.code != 0
        {
            if is_missing_blobs(status.code) {
                // The CAS lost something between our upload and execution.
                // Drop what we believe the server has, so the next attempt
                // re-uploads rather than deciding everything is present.
                self.cas.forget_known_present();
            }
            bail!(
                "Remote execution of job {job_id} failed: {} (code {}){}",
                status.message,
                status.code,
                if response.message.is_empty() {
                    String::new()
                } else {
                    format!(": {}", response.message)
                }
            );
        }

        let result = response
            .result
            .context("Remote execution returned no action result")?;

        if response.cached_result {
            trace!("[{job_id}]: remote action cache hit");
        }

        let output = self.collect_outputs(result, &plan.output_paths).await?;
        Ok((
            RunJobResult::Complete(JobComplete {
                output: output.output,
                outputs: output.outputs,
            }),
            path_transformer,
        ))
    }

    async fn put_toolchain(
        &self,
        compiler_path: PathBuf,
        weak_key: String,
        toolchain_packager: Box<dyn ToolchainPackager>,
    ) -> Result<(Toolchain, Option<(String, PathBuf)>)> {
        if self.reapi_config.toolchain == ReapiToolchainMode::Image {
            // Packaging a toolchain means running the compiler several times
            // and tarring up its shared libraries. When the compiler is baked
            // into the worker's image none of that work is ever used, so skip
            // it entirely and identify the toolchain by its weak key.
            return Ok((
                Toolchain {
                    archive_id: weak_key,
                },
                None,
            ));
        }
        let tc_cache = self.tc_cache.clone();
        self.pool
            .spawn_blocking(move || {
                tc_cache.put_toolchain(&compiler_path, &weak_key, toolchain_packager)
            })
            .await?
    }

    fn rewrite_includes_only(&self) -> bool {
        self.rewrite_includes_only
    }

    fn get_custom_toolchain(&self, exe: &Path) -> Option<PathBuf> {
        match self.tc_cache.get_custom_toolchain(exe) {
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
