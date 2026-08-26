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

//! End-to-end tests against a fake REv2 service.
//!
//! The fake implements CAS, ByteStream, Capabilities and Execution for real:
//! it materializes the input root into a temporary directory, runs the
//! command, and collects the declared outputs. That makes the whole client
//! path -- Merkle tree construction, blob upload, path rebasing, action
//! execution, output retrieval -- testable without a network, a container
//! runtime, or a buildbarn deployment.

#![allow(clippy::result_large_err)] // `tonic::Status` is large; the API is not ours.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tonic::{Request, Response, Status};

use crate::{
    config::{DistNetworking, ReapiConfig, ReapiToolchainMode},
    dist::{Client as _, CompileCommand, RunJobResponse, Toolchain},
};

use super::{
    merkle::digest_bytes,
    proto::{
        build::bazel::remote::execution::v2 as reapi,
        google::{
            bytestream::{
                self, byte_stream_server::ByteStream, byte_stream_server::ByteStreamServer,
            },
            longrunning::Operation,
        },
    },
};

use reapi::{
    capabilities_server::{Capabilities, CapabilitiesServer},
    content_addressable_storage_server::{
        ContentAddressableStorage, ContentAddressableStorageServer,
    },
    execution_server::{Execution, ExecutionServer},
};

// ---------------------------------------------------------------------------
// A fake REv2 service
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeState {
    blobs: HashMap<String, Vec<u8>>,
    /// Every action digest the server was asked to execute, in order.
    executed: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeServer {
    state: Arc<Mutex<FakeState>>,
}

impl FakeServer {
    fn put(&self, data: Vec<u8>) -> reapi::Digest {
        let digest = digest_bytes(&data);
        self.state
            .lock()
            .unwrap()
            .blobs
            .insert(digest.hash.clone(), data);
        digest
    }

    fn get(&self, digest: &reapi::Digest) -> Result<Vec<u8>, Status> {
        self.state
            .lock()
            .unwrap()
            .blobs
            .get(&digest.hash)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no blob {}", digest.hash)))
    }

    fn decode<M: prost::Message + Default>(&self, digest: &reapi::Digest) -> Result<M, Status> {
        M::decode(&self.get(digest)?[..])
            .map_err(|e| Status::invalid_argument(format!("undecodable blob: {e}")))
    }

    /// Write a `Directory` tree out to disk, exactly as a worker would.
    fn materialize(&self, digest: &reapi::Digest, root: &std::path::Path) -> Result<(), Status> {
        let dir: reapi::Directory = self.decode(digest)?;
        std::fs::create_dir_all(root)?;

        for file in &dir.files {
            let digest = file
                .digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("file node without a digest"))?;
            let path = root.join(&file.name);
            std::fs::write(&path, self.get(digest)?)?;
            if file.is_executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        for link in &dir.symlinks {
            std::os::unix::fs::symlink(&link.target, root.join(&link.name))?;
        }
        for child in &dir.directories {
            let digest = child
                .digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("directory node without a digest"))?;
            self.materialize(digest, &root.join(&child.name))?;
        }
        Ok(())
    }

    fn run_action(&self, action_digest: &reapi::Digest) -> Result<reapi::ExecuteResponse, Status> {
        let action: reapi::Action = self.decode(action_digest)?;
        let command: reapi::Command = self.decode(
            action
                .command_digest
                .as_ref()
                .ok_or_else(|| Status::invalid_argument("action without a command"))?,
        )?;
        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("action without an input root"))?;

        self.state
            .lock()
            .unwrap()
            .executed
            .push(action_digest.hash.clone());

        let build_dir = tempfile::tempdir()?;
        let root = build_dir.path();
        self.materialize(input_root, root)?;

        // REv2: the working directory "must be a directory which exists in the
        // input tree". Enforced rather than created, because creating it would
        // hide a client that forgot to put it there -- which is exactly the
        // bug this once was.
        let cwd = root.join(&command.working_directory);
        if !cwd.is_dir() {
            return Err(Status::failed_precondition(format!(
                "working directory {:?} is not in the input root",
                command.working_directory
            )));
        }

        // Directories leading up to the output files are created by the
        // worker, per the spec, even when they are not in the input root.
        for output in &command.output_paths {
            if let Some(parent) = PathBuf::from(output).parent() {
                std::fs::create_dir_all(cwd.join(parent))?;
            }
        }

        let mut process = std::process::Command::new(&command.arguments[0]);
        process
            .args(&command.arguments[1..])
            .current_dir(&cwd)
            .env_clear();
        for env in &command.environment_variables {
            process.env(&env.name, &env.value);
        }

        // Retry ETXTBSY.
        //
        // This fake worker materializes an executable and runs it inside a
        // multithreaded test process. If another thread happens to `fork()`
        // while a write handle to that file is still open, the child inherits
        // the descriptor and `execve` fails with ETXTBSY. A real REv2 worker
        // sets up its input root in a dedicated process and never hits this;
        // it is purely an artifact of the harness, so retry rather than let it
        // flake the suite.
        let mut attempt = 0;
        let output = loop {
            match process.output() {
                Ok(output) => break output,
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempt < 50 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => {
                    return Err(Status::internal(format!(
                        "could not spawn {:?}: {e}",
                        command.arguments
                    )));
                }
            }
        };

        let mut output_files = Vec::new();
        for path in &command.output_paths {
            match std::fs::read(cwd.join(path)) {
                Ok(data) => output_files.push(reapi::OutputFile {
                    path: path.clone(),
                    digest: Some(self.put(data)),
                    is_executable: false,
                    contents: Vec::new(),
                    node_properties: None,
                }),
                // REv2 permits an action to not produce a declared output.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
        }

        Ok(reapi::ExecuteResponse {
            result: Some(reapi::ActionResult {
                output_files,
                exit_code: output.status.code().unwrap_or(-1),
                stdout_raw: output.stdout,
                stderr_raw: output.stderr,
                ..Default::default()
            }),
            cached_result: false,
            status: None,
            server_logs: Default::default(),
            message: String::new(),
        })
    }
}

#[tonic::async_trait]
impl Capabilities for FakeServer {
    async fn get_capabilities(
        &self,
        _: Request<reapi::GetCapabilitiesRequest>,
    ) -> Result<Response<reapi::ServerCapabilities>, Status> {
        Ok(Response::new(reapi::ServerCapabilities {
            cache_capabilities: Some(reapi::CacheCapabilities {
                digest_functions: vec![reapi::digest_function::Value::Sha256 as i32],
                // Deliberately small, so the tests exercise both the batch and
                // the ByteStream upload paths.
                max_batch_total_size_bytes: 128 * 1024,
                ..Default::default()
            }),
            execution_capabilities: Some(reapi::ExecutionCapabilities {
                digest_function: reapi::digest_function::Value::Sha256 as i32,
                exec_enabled: true,
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for FakeServer {
    async fn find_missing_blobs(
        &self,
        request: Request<reapi::FindMissingBlobsRequest>,
    ) -> Result<Response<reapi::FindMissingBlobsResponse>, Status> {
        let state = self.state.lock().unwrap();
        Ok(Response::new(reapi::FindMissingBlobsResponse {
            missing_blob_digests: request
                .into_inner()
                .blob_digests
                .into_iter()
                .filter(|d| !state.blobs.contains_key(&d.hash))
                .collect(),
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<reapi::BatchUpdateBlobsRequest>,
    ) -> Result<Response<reapi::BatchUpdateBlobsResponse>, Status> {
        use reapi::batch_update_blobs_response::Response as BlobResponse;

        let mut responses = Vec::new();
        for blob in request.into_inner().requests {
            let digest = self.put(blob.data);
            responses.push(BlobResponse {
                digest: Some(digest),
                status: None,
            });
        }
        Ok(Response::new(reapi::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<reapi::BatchReadBlobsRequest>,
    ) -> Result<Response<reapi::BatchReadBlobsResponse>, Status> {
        use reapi::batch_read_blobs_response::Response as BlobResponse;

        let mut responses = Vec::new();
        for digest in request.into_inner().digests {
            let data = self.get(&digest)?;
            responses.push(BlobResponse {
                digest: Some(digest),
                data,
                compressor: reapi::compressor::Value::Identity as i32,
                status: None,
            });
        }
        Ok(Response::new(reapi::BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = futures::stream::Empty<Result<reapi::GetTreeResponse, Status>>;
    async fn get_tree(
        &self,
        _: Request<reapi::GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("GetTree"))
    }

    async fn split_blob(
        &self,
        _: Request<reapi::SplitBlobRequest>,
    ) -> Result<Response<reapi::SplitBlobResponse>, Status> {
        Err(Status::unimplemented("SplitBlob"))
    }

    async fn splice_blob(
        &self,
        _: Request<reapi::SpliceBlobRequest>,
    ) -> Result<Response<reapi::SpliceBlobResponse>, Status> {
        Err(Status::unimplemented("SpliceBlob"))
    }
}

#[tonic::async_trait]
impl ByteStream for FakeServer {
    type ReadStream = futures::stream::BoxStream<'static, Result<bytestream::ReadResponse, Status>>;

    async fn read(
        &self,
        request: Request<bytestream::ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        use futures::StreamExt;

        // "{instance}/blobs/{hash}/{size}"
        let name = request.into_inner().resource_name;
        let hash = name
            .split('/')
            .rev()
            .nth(1)
            .ok_or_else(|| Status::invalid_argument(format!("bad resource name {name:?}")))?;
        let data = self
            .state
            .lock()
            .unwrap()
            .blobs
            .get(hash)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("no blob {hash}")))?;

        // Deliberately chunked, so the client's reassembly is exercised.
        let chunks: Vec<Result<bytestream::ReadResponse, Status>> = data
            .chunks(1024)
            .map(|chunk| {
                Ok(bytestream::ReadResponse {
                    data: chunk.to_vec(),
                })
            })
            .collect();
        Ok(Response::new(futures::stream::iter(chunks).boxed()))
    }

    async fn write(
        &self,
        request: Request<tonic::Streaming<bytestream::WriteRequest>>,
    ) -> Result<Response<bytestream::WriteResponse>, Status> {
        use futures::StreamExt;

        let mut stream = request.into_inner();
        let mut data = Vec::new();
        let mut resource_name = String::new();

        while let Some(message) = stream.next().await {
            let message = message?;
            if !message.resource_name.is_empty() {
                resource_name = message.resource_name;
            }
            if message.write_offset != data.len() as i64 {
                return Err(Status::invalid_argument(format!(
                    "write offset {} does not continue from {}",
                    message.write_offset,
                    data.len()
                )));
            }
            data.extend_from_slice(&message.data);
        }

        // "{instance}/uploads/{uuid}/blobs/{hash}/{size}"
        let expected = resource_name
            .split('/')
            .rev()
            .nth(1)
            .ok_or_else(|| Status::invalid_argument("bad upload resource name"))?
            .to_owned();
        let committed_size = data.len() as i64;
        let digest = self.put(data);
        if digest.hash != expected {
            return Err(Status::invalid_argument(format!(
                "uploaded data hashes to {} but was named {expected}",
                digest.hash
            )));
        }

        Ok(Response::new(bytestream::WriteResponse { committed_size }))
    }

    async fn query_write_status(
        &self,
        _: Request<bytestream::QueryWriteStatusRequest>,
    ) -> Result<Response<bytestream::QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("QueryWriteStatus"))
    }
}

#[tonic::async_trait]
impl Execution for FakeServer {
    type ExecuteStream = futures::stream::BoxStream<'static, Result<Operation, Status>>;
    type WaitExecutionStream = Self::ExecuteStream;

    async fn execute(
        &self,
        request: Request<reapi::ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        use futures::StreamExt;
        use prost::Message;

        let request = request.into_inner();
        let action_digest = request
            .action_digest
            .ok_or_else(|| Status::invalid_argument("no action digest"))?;

        let response = self.run_action(&action_digest)?;

        // Report a queued operation first, then the completed one, so the
        // client's stream handling is exercised rather than short-circuited.
        let queued = Operation {
            name: format!("operations/{}", action_digest.hash),
            metadata: Some(prost_types::Any {
                type_url: "type.googleapis.com/build.bazel.remote.execution.v2.\
                           ExecuteOperationMetadata"
                    .to_owned(),
                value: reapi::ExecuteOperationMetadata {
                    stage: reapi::execution_stage::Value::Queued as i32,
                    action_digest: Some(action_digest.clone()),
                    ..Default::default()
                }
                .encode_to_vec(),
            }),
            done: false,
            result: None,
        };
        let done = Operation {
            name: format!("operations/{}", action_digest.hash),
            metadata: None,
            done: true,
            result: Some(
                super::proto::google::longrunning::operation::Result::Response(prost_types::Any {
                    type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
                        .to_owned(),
                    value: response.encode_to_vec(),
                }),
            ),
        };

        Ok(Response::new(
            futures::stream::iter(vec![Ok(queued), Ok(done)]).boxed(),
        ))
    }

    async fn wait_execution(
        &self,
        _: Request<reapi::WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        Err(Status::unimplemented("WaitExecution"))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Fixture {
    client: super::Client,
    server: FakeServer,
    _cache_dir: tempfile::TempDir,
}

async fn fixture(toolchain: ReapiToolchainMode) -> Fixture {
    let server = FakeServer::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn({
        let server = server.clone();
        async move {
            tonic::transport::Server::builder()
                .add_service(CapabilitiesServer::new(server.clone()))
                .add_service(ContentAddressableStorageServer::new(server.clone()))
                .add_service(ByteStreamServer::new(server.clone()))
                .add_service(ExecutionServer::new(server))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
        }
    });

    let cache_dir = tempfile::tempdir().unwrap();
    let client = super::Client::new(
        ReapiConfig {
            url: Some(format!("grpc://{addr}")),
            instance_name: "test".to_owned(),
            toolchain,
            stage: crate::config::ReapiStageMode::Preprocessed,
            platform: [("OSFamily".to_owned(), "Linux".to_owned())]
                .into_iter()
                .collect(),
            headers: [("x-test-header".to_owned(), "sccache".to_owned())]
                .into_iter()
                .collect(),
            action_timeout_secs: 60,
            skip_cache_lookup: false,
            do_not_cache: false,
            env_passthrough: vec!["SOURCE_DATE_EPOCH".to_owned()],
        },
        cache_dir.path(),
        1024 * 1024 * 1024,
        &[],
        None,
        true,
        0f64,
        false,
        &DistNetworking::default(),
    )
    .await
    .expect("could not create the REAPI client");

    Fixture {
        client,
        server,
        _cache_dir: cache_dir,
    }
}

/// Build the zlib-compressed tar that `InputsPackager` would have produced.
fn inputs_tar(entries: &[(&str, &[u8], u32)]) -> std::fs::File {
    let file = tempfile::tempfile().unwrap();
    let mut builder = tar::Builder::new(flate2::write::ZlibEncoder::new(
        file.try_clone().unwrap(),
        flate2::Compression::fast(),
    ));
    for (path, data, mode) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        // Paths are root-relative, exactly as `pkg::tar_safe_path` leaves them.
        builder.append_data(&mut header, path, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap();

    use std::io::{Seek, SeekFrom};
    let mut file = file;
    file.seek(SeekFrom::Start(0)).unwrap();
    file
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A "compiler" that is really a shell script, shipped in the input root. It
/// exercises the parts that matter: a relative `argv[0]`, the executable bit
/// surviving the Merkle tree, a working directory below the input root, and a
/// relative output path.
const FAKE_COMPILER: &[u8] = br#"#!/bin/sh
# usage: cc -o <out> <in>
out=""
input=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) input="$1"; shift ;;
  esac
done
echo "compiling $input" >&2
printf 'OBJECT:' > "$out"
cat "$input" >> "$out"
exit 0
"#;

fn compile_command() -> CompileCommand {
    CompileCommand {
        executable: "/opt/cc/bin/cc".to_owned(),
        arguments: vec![
            "-O2".to_owned(),
            "-o".to_owned(),
            "/work/proj/build/a.o".to_owned(),
            "/work/proj/src/a.dist.cpp".to_owned(),
        ],
        env_vars: vec![
            ("SOURCE_DATE_EPOCH".to_owned(), "0".to_owned()),
            ("AWS_SECRET_ACCESS_KEY".to_owned(), "hunter2".to_owned()),
        ],
        cwd: "/work/proj/build".to_owned(),
    }
}

/// Ship the compiler in the input root, rather than in a separate toolchain
/// archive, so the test does not need sccache's toolchain packager.
async fn run_one(fixture: &Fixture, source: &[u8]) -> RunJobResponse {
    let inputs = inputs_tar(&[
        ("work/proj/src/a.dist.cpp", source, 0o644),
        ("opt/cc/bin/cc", FAKE_COMPILER, 0o755),
    ]);

    let toolchain = Toolchain {
        archive_id: "test-toolchain".to_owned(),
    };
    let job = fixture
        .client
        .new_job(toolchain.clone(), crate::dist::JobInputs::Archive(inputs))
        .await
        .unwrap();

    // The compiler travelled with the inputs, so nothing else has to be sent.
    fixture
        .client
        .toolchains
        .lock()
        .await
        .insert(toolchain.archive_id.clone(), Default::default());

    fixture
        .client
        .run_job(
            &job.job_id,
            std::time::Duration::from_secs(60),
            toolchain,
            compile_command(),
            vec!["/work/proj/build/a.o".to_owned()],
        )
        .await
        .unwrap()
}

/// The same inputs as [`run_one`], but as real files on disk described by
/// `InputEntry`s -- the archive-free path.
fn inputs_entries(dir: &Path, source: &[u8]) -> Vec<crate::dist::pkg::InputEntry> {
    use crate::dist::pkg::InputEntry;
    use std::io::Write;

    let write = |name: &str, data: &[u8], mode: u32| -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(data).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        path
    };

    vec![
        InputEntry::File {
            dist_path: PathBuf::from("work/proj/src/a.dist.cpp"),
            src_path: write("a.dist.cpp", source, 0o644),
        },
        InputEntry::File {
            dist_path: PathBuf::from("opt/cc/bin/cc"),
            src_path: write("cc", FAKE_COMPILER, 0o755),
        },
    ]
}

async fn run_one_from_entries(
    fixture: &Fixture,
    entries: &Arc<Vec<crate::dist::pkg::InputEntry>>,
) -> RunJobResponse {
    let toolchain = Toolchain {
        archive_id: "test-toolchain".to_owned(),
    };
    let job = fixture
        .client
        .new_job(
            toolchain.clone(),
            crate::dist::JobInputs::Entries(entries.clone()),
        )
        .await
        .unwrap();

    fixture
        .client
        .toolchains
        .lock()
        .await
        .insert(toolchain.archive_id.clone(), Default::default());

    fixture
        .client
        .run_job(
            &job.job_id,
            std::time::Duration::from_secs(60),
            toolchain,
            compile_command(),
            vec!["/work/proj/build/a.o".to_owned()],
        )
        .await
        .unwrap()
}

/// The two input representations have to describe byte-identical input roots.
///
/// If they diverge, the action digest diverges, and a build that switches
/// between them silently loses every remote action cache hit -- which is the
/// kind of regression that shows up as "remote execution got slower" long
/// after the change that caused it.
#[tokio::test]
async fn entries_and_archive_agree_on_the_input_root() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(ReapiToolchainMode::Inputs).await;

    let entries = Arc::new(inputs_entries(dir.path(), b"the source text"));
    let from_archive = run_one(&fixture, b"the source text").await;
    let from_entries = run_one_from_entries(&fixture, &entries).await;

    let executed = fixture.server.state.lock().unwrap().executed.clone();
    assert_eq!(executed.len(), 2);
    assert_eq!(
        executed[0], executed[1],
        "staging inputs individually produced a different action digest than the archive"
    );

    for response in [from_archive, from_entries] {
        let RunJobResponse::Complete { result, .. } = response else {
            panic!("expected a completed job, got {response:?}");
        };
        assert_eq!(result.output.code(), Some(0));
    }
}

/// A header shared by many compilations is hashed once, not once per job.
#[tokio::test]
async fn repeated_inputs_are_hashed_once() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = fixture(ReapiToolchainMode::Inputs).await;

    // The same files, referenced by two successive jobs -- exactly how a
    // build reuses a header across translation units.
    let entries = Arc::new(inputs_entries(dir.path(), b"same source"));

    run_one_from_entries(&fixture, &entries).await;
    // Two distinct files (source + compiler) seen, so two cache entries.
    assert_eq!(fixture.client.digests.len(), 2);

    run_one_from_entries(&fixture, &entries).await;
    // The second job reused both; nothing new was hashed.
    assert_eq!(fixture.client.digests.len(), 2);
}

#[tokio::test]
async fn compiles_remotely_end_to_end() {
    use std::io::Read;

    let fixture = fixture(ReapiToolchainMode::Inputs).await;
    let response = run_one(&fixture, b"the source text").await;

    let RunJobResponse::Complete { result, .. } = response else {
        panic!("expected a completed job, got {response:?}");
    };

    assert_eq!(result.output.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&result.output.stderr),
        "compiling ../src/a.dist.cpp\n"
    );

    // Exactly one output, mapped back to the absolute path sccache asked for.
    assert_eq!(result.outputs.len(), 1);
    assert_eq!(result.outputs[0].0, "/work/proj/build/a.o");

    let mut object = Vec::new();
    result.outputs[0]
        .1
        .clone()
        .into_reader()
        .read_to_end(&mut object)
        .unwrap();
    assert_eq!(object, b"OBJECT:the source text");
}

#[tokio::test]
async fn identical_work_produces_one_action_digest() {
    let fixture = fixture(ReapiToolchainMode::Inputs).await;

    run_one(&fixture, b"same source").await;
    run_one(&fixture, b"same source").await;
    run_one(&fixture, b"different source").await;

    let executed = fixture.server.state.lock().unwrap().executed.clone();
    assert_eq!(executed.len(), 3);
    // Identical inputs must hash identically, or the server's action cache is
    // worthless. Different inputs must not.
    assert_eq!(executed[0], executed[1]);
    assert_ne!(executed[0], executed[2]);
}

#[tokio::test]
async fn blobs_are_only_uploaded_once() {
    let fixture = fixture(ReapiToolchainMode::Inputs).await;

    assert_eq!(fixture.server.state.lock().unwrap().blobs.len(), 0);

    run_one(&fixture, b"some source").await;
    let after_first = fixture.server.state.lock().unwrap().blobs.len();
    // Source, compiler, the Directory blobs, Command, Action, and the object
    // the action produced. The exact count matters less than the fact that the
    // upload actually happened: an earlier version of this client silently
    // uploaded nothing, because `File::try_clone` shares the file cursor and
    // the second pass over the archive started at EOF.
    assert!(after_first > 5, "nothing was uploaded: {after_first} blobs");

    run_one(&fixture, b"some source").await;
    let after_second = fixture.server.state.lock().unwrap().blobs.len();

    // The second identical compilation re-uploads nothing.
    assert_eq!(after_first, after_second);
}

#[tokio::test]
async fn large_inputs_take_the_streaming_path() {
    use std::io::Read;

    // The fake server advertises a 128 KiB batch limit, so a 512 KiB source
    // file cannot be sent with BatchUpdateBlobs and must go through
    // ByteStream.Write. Real preprocessed C++ routinely exceeds the 4 MiB
    // limit that servers actually advertise -- `CommandLine.cpp` preprocesses
    // to 3.6 MiB -- so this path is not an edge case.
    let source = vec![b'x'; 512 * 1024];
    let fixture = fixture(ReapiToolchainMode::Inputs).await;
    let response = run_one(&fixture, &source).await;

    let RunJobResponse::Complete { result, .. } = response else {
        panic!("expected a completed job, got {response:?}");
    };

    let mut object = Vec::new();
    result.outputs[0]
        .1
        .clone()
        .into_reader()
        .read_to_end(&mut object)
        .unwrap();
    assert_eq!(object.len(), b"OBJECT:".len() + source.len());
}

#[tokio::test]
async fn compiler_failure_is_reported_as_a_completed_job() {
    // A nonzero compiler exit status is a failed *build*, not a failed remote
    // execution: it has to come back as Complete so sccache reports the
    // compiler's own diagnostics rather than retrying or falling back.
    let fixture = fixture(ReapiToolchainMode::Inputs).await;

    let inputs = inputs_tar(&[
        ("work/proj/src/a.dist.cpp", b"source", 0o644),
        (
            "opt/cc/bin/cc",
            b"#!/bin/sh\necho 'error: no' >&2\nexit 1\n",
            0o755,
        ),
    ]);
    let toolchain = Toolchain {
        archive_id: "failing".to_owned(),
    };
    let job = fixture
        .client
        .new_job(toolchain.clone(), crate::dist::JobInputs::Archive(inputs))
        .await
        .unwrap();
    fixture
        .client
        .toolchains
        .lock()
        .await
        .insert(toolchain.archive_id.clone(), Default::default());

    let response = fixture
        .client
        .run_job(
            &job.job_id,
            std::time::Duration::from_secs(60),
            toolchain,
            compile_command(),
            vec!["/work/proj/build/a.o".to_owned()],
        )
        .await
        .unwrap();

    let RunJobResponse::Complete { result, .. } = response else {
        panic!("expected a completed job, got {response:?}");
    };
    assert_eq!(result.output.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&result.output.stderr),
        "error: no\n"
    );
    assert!(result.outputs.is_empty());
}

#[tokio::test]
async fn an_unknown_job_reports_missing_inputs() {
    let fixture = fixture(ReapiToolchainMode::Inputs).await;
    let response = fixture
        .client
        .run_job(
            "no-such-job",
            std::time::Duration::from_secs(60),
            Toolchain {
                archive_id: "whatever".to_owned(),
            },
            compile_command(),
            vec!["/work/proj/build/a.o".to_owned()],
        )
        .await
        .unwrap();

    assert!(
        matches!(response, RunJobResponse::MissingJobInputs { .. }),
        "got {response:?}"
    );
}

#[tokio::test]
async fn image_mode_sends_no_toolchain() {
    let fixture = fixture(ReapiToolchainMode::Image).await;

    let job = fixture
        .client
        .new_job(
            Toolchain {
                archive_id: "in-the-image".to_owned(),
            },
            crate::dist::JobInputs::Archive(inputs_tar(&[(
                "work/proj/src/a.dist.cpp",
                b"source",
                0o644,
            )])),
        )
        .await
        .unwrap();

    // Nothing to upload, so the retry loop never calls put_toolchain.
    assert!(job.has_toolchain);
    assert!(job.has_inputs);
}

#[tokio::test]
async fn get_status_rejects_a_cache_only_endpoint() {
    // Pointing sccache at a CAS with no execution service is an easy
    // misconfiguration, and it should fail loudly at startup rather than on
    // the first compile.
    #[derive(Clone)]
    struct CacheOnly;

    #[tonic::async_trait]
    impl Capabilities for CacheOnly {
        async fn get_capabilities(
            &self,
            _: Request<reapi::GetCapabilitiesRequest>,
        ) -> Result<Response<reapi::ServerCapabilities>, Status> {
            Ok(Response::new(reapi::ServerCapabilities {
                cache_capabilities: Some(reapi::CacheCapabilities {
                    digest_functions: vec![reapi::digest_function::Value::Sha256 as i32],
                    max_batch_total_size_bytes: 4 * 1024 * 1024,
                    ..Default::default()
                }),
                execution_capabilities: None,
                ..Default::default()
            }))
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(CapabilitiesServer::new(CacheOnly))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    let cache_dir = tempfile::tempdir().unwrap();
    let result = super::Client::new(
        ReapiConfig {
            url: Some(format!("grpc://{addr}")),
            ..Default::default()
        },
        cache_dir.path(),
        1024 * 1024,
        &[],
        None,
        true,
        0f64,
        false,
        &DistNetworking::default(),
    )
    .await
    .unwrap()
    .get_status()
    .await;

    let err = format!("{:#}", result.unwrap_err());
    assert!(err.contains("cache-only"), "unexpected error: {err}");
}
