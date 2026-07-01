use crate::{
    cache::{CacheMode, GetPathResult, StorageKind},
    compiler::ColorMode,
    config::PreprocessorCacheModeConfig,
    mock_command::ProcessOutput,
    server::{DistInfo, ServerInfo, ServerStats},
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;

/// A client request.
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Zero the server's statistics.
    ZeroStats,
    /// Get server statistics.
    GetStats,
    /// Get dist status.
    DistStatus,
    /// Shut the server down gracefully.
    Shutdown,
    /// Execute a compile or fetch a cached compilation result.
    Compile(Compile),

    // --- Storage RPCs (client-side mode) ---
    /// One-shot handshake: client requests cache metadata from the daemon.
    StorageHandshake {
        kind: StorageKind,
    },
    /// Fetch the filesystem path of the cached entry for `key`.
    /// Returns `None` if the backend does not support direct file access or the key is absent.
    StorageGetPath {
        key: String,
        kind: StorageKind,
    },
    // Delete the file for the cached entry for `key`
    StorageDelPath {
        key: String,
        kind: StorageKind,
    },
    /// Fetch raw (zip) bytes for `key`; returns `None` on a miss.
    StorageGetRaw {
        key: String,
        kind: StorageKind,
    },
    /// Store raw (zip) bytes under `key`.
    StoragePutRaw {
        key: String,
        data: Vec<u8>,
        kind: StorageKind,
    },
    /// Merge per-invocation stats into the daemon's running totals.
    RecordStats(Box<ServerStats>),
}

/// A server response.
#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    /// Response for `Request::Compile`.
    Compile(CompileResponse),
    /// Response for `Request::ZeroStats`.
    ZeroStats,
    /// Response for `Request::GetStats`, containing server statistics.
    Stats(Box<ServerInfo>),
    /// Response for `Request::DistStatus`, containing client info.
    DistStatus(DistInfo),
    /// Response for `Request::Shutdown`, containing server statistics.
    ShuttingDown(Box<ServerInfo>),
    /// Second response for `Request::Compile`, containing the results of the compilation.
    CompileFinished(CompileFinished),

    // --- Storage RPC responses (client-side mode) ---
    /// Response for `Request::StorageHandshake`.
    StorageHandshake(StorageHandshakeInfo),
    /// Response for `Request::StorageGetPath`.
    StorageGetPath(GetPathResult),
    /// Response for `Request::StorageDelPath`.
    StorageDelPath(Result<(), String>),
    /// Response for `Request::StorageGetRaw`: zip bytes on hit, `None` on miss.
    StorageGetRaw(Option<Vec<u8>>),
    /// Response for `Request::StoragePutRaw`.
    StoragePutRaw(Result<(), String>),
    /// Response for `Request::RecordStats`.
    RecordStats,
}

/// Possible responses from the server for a `Compile` request.
#[derive(Serialize, Deserialize, Debug)]
pub enum CompileResponse {
    /// The compilation was started.
    CompileStarted,
    /// The server could not handle this compilation request.
    UnhandledCompile,
    /// The compiler was not supported.
    UnsupportedCompiler(OsString),
}

/// Information about a finished compile, either from cache or executed locally.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CompileFinished {
    /// The state of any compiler options passed to control color output.
    pub color_mode: ColorMode,
    /// The output of the compile process.
    pub output: ProcessOutput,
}

/// The contents of a compile request from a client.
#[derive(Serialize, Deserialize, Debug, Clone, Hash, Eq, PartialEq)]
pub struct Compile {
    /// The current working directory in which to execute the compile.
    pub cwd: OsString,
    /// The full path to the compiler executable.
    pub exe: OsString,
    /// The commandline arguments passed to the compiler.
    pub args: Vec<OsString>,
    /// The environment variables present when the compiler was executed, as (var, val).
    pub env_vars: Vec<(OsString, OsString)>,
}

/// Cache metadata returned by the daemon on `StorageHandshake`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageHandshakeInfo {
    pub location: String,
    pub cache_type_name: String,
    pub basedirs: Vec<Vec<u8>>,
    pub preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
    pub cache_mode: CacheMode,
    pub max_size: Option<u64>,
}
