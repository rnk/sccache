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
    cache::{Cache, FileObjectSource, Storage},
    compiler::{
        Cacheable, ColorMode, Compilation, CompileCommand, CompileCommandImpl, Compiler,
        CompilerArguments, CompilerHasher, CompilerKind, HashResult, Language,
        preprocessor_cache::{
            CachedIncludeEntry, include_is_too_new, preprocessor_cache_entry_hash_key,
        },
    },
    config::PreprocessorCacheModeConfig,
    dist,
    errors::*,
    lru_disk_cache::LruCache,
    mock_command::{CommandCreatorSync, ProcessOutput},
    server::SccacheService,
    util::{Digest, HASH_BUFFER_SIZE, HashToDigest, hash_all, read_line_batches, strip_basedirs},
};
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryFutureExt, TryStreamExt, lock::Mutex};
use itertools::Itertools;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fmt,
    hash::Hash,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};
use tempfile::TempPath;

#[cfg(feature = "dist-client")]
use crate::{
    compiler::{DistPackagers, NoopOutputsRewriter},
    dist::pkg::{self, InputsWriter},
};

use super::CacheControl;
use super::preprocessor_cache::PreprocessorCacheEntry;

/// A generic implementation of the `Compiler` trait for C/C++ compilers.
#[derive(Clone)]
pub struct CCompiler<I>
where
    I: CCompilerImpl,
{
    executable: PathBuf,
    #[cfg(test)]
    pub executable_digest: String,
    #[cfg(not(test))]
    executable_digest: String,
    compiler: I,
    // cache for hashed preprocessor dependencies
    preprocessor_dependencies_cache: Arc<Mutex<LruCache<PathBuf, Arc<Result<CachedIncludeEntry>>>>>,
}

/// A generic implementation of the `CompilerHasher` trait for C/C++ compilers.
#[derive(Debug, Clone)]
pub struct CCompilerHasher<I>
where
    I: CCompilerImpl,
{
    parsed_args: ParsedArguments,
    executable: PathBuf,
    executable_digest: String,
    compiler: I,
    // cache for hashed preprocessor dependencies
    preprocessor_dependencies_cache: Arc<Mutex<LruCache<PathBuf, Arc<Result<CachedIncludeEntry>>>>>,
}

/// Artifact produced by a C/C++ compiler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDescriptor {
    /// Path to the artifact.
    pub path: PathBuf,
    /// Whether the artifact is an optional object file.
    pub optional: bool,
}

#[derive(Debug)]
pub enum OutDir {
    Dir(PathBuf),
    Tmp(tempfile::TempDir),
}

impl OutDir {
    pub fn path(&self) -> &Path {
        match self {
            Self::Dir(dir) => dir.as_path(),
            Self::Tmp(dir) => dir.path(),
        }
    }
}

impl Eq for OutDir {}

impl PartialEq for OutDir {
    fn eq(&self, other: &Self) -> bool {
        self.path() == other.path()
    }
}

impl AsRef<Path> for OutDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

/// The results of parsing a compiler commandline.
#[allow(dead_code)]
#[derive(Default, Debug, Clone)]
pub struct ParsedArguments {
    /// The input source file.
    pub input: PathBuf,
    /// Whether to prepend the input with `--`
    pub double_dash_input: bool,
    /// The type of language used in the input source file.
    pub language: Language,
    /// The flag required to compile for the given language
    pub compilation_flag: OsString,
    /// Whether C++20 modules are enabled
    pub cxx20_modules: bool,
    /// The file in which to generate dependencies.
    pub depfile: Option<PathBuf>,
    /// Output files and whether it's optional, keyed by a simple name, like "obj".
    pub outputs: HashMap<&'static str, ArtifactDescriptor>,
    /// Commandline arguments for dependency generation.
    pub dependency_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor (not including common_args).
    pub preprocessor_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor or the compiler.
    pub common_args: Vec<OsString>,
    /// Commandline arguments for the compiler that specify the architecture given
    pub arch_args: Vec<OsString>,
    /// Commandline arguments for the preprocessor or the compiler that don't affect the computed hash.
    pub unhashed_args: Vec<OsString>,
    /// Extra unhashed files that need to be sent along with dist compiles.
    pub extra_dist_files: Vec<PathBuf>,
    /// Extra files that need to have their contents hashed.
    pub extra_hash_files: Vec<PathBuf>,
    /// Whether or not the `-showIncludes` argument is passed on MSVC
    pub msvc_show_includes: bool,
    /// Whether the compilation is generating profiling or coverage data.
    pub profile_generate: bool,
    /// The color mode.
    pub color_mode: ColorMode,
    /// arguments are incompatible with rewrite_includes_only
    pub suppress_rewrite_includes_only: bool,
    /// Arguments are incompatible with preprocessor cache mode
    pub too_hard_for_preprocessor_cache_mode: Vec<OsString>,
    /// Path to the scratch directory to use for this compilation (nvcc)
    pub out_dir: Option<Arc<OutDir>>,
}

impl std::cmp::Eq for ParsedArguments {}

impl std::cmp::PartialEq for ParsedArguments {
    fn eq(&self, other: &Self) -> bool {
        let ParsedArguments {
            input,
            double_dash_input,
            language,
            compilation_flag,
            cxx20_modules,
            depfile,
            outputs,
            dependency_args,
            preprocessor_args,
            common_args,
            arch_args,
            unhashed_args,
            extra_dist_files,
            extra_hash_files,
            msvc_show_includes,
            profile_generate,
            color_mode,
            suppress_rewrite_includes_only,
            too_hard_for_preprocessor_cache_mode,
            out_dir,
        } = self;
        input == &other.input
            && double_dash_input == &other.double_dash_input
            && language == &other.language
            && compilation_flag == &other.compilation_flag
            && cxx20_modules == &other.cxx20_modules
            && depfile == &other.depfile
            && outputs == &other.outputs
            && dependency_args == &other.dependency_args
            && preprocessor_args == &other.preprocessor_args
            && common_args == &other.common_args
            && arch_args == &other.arch_args
            && unhashed_args == &other.unhashed_args
            && extra_dist_files == &other.extra_dist_files
            && extra_hash_files == &other.extra_hash_files
            && msvc_show_includes == &other.msvc_show_includes
            && profile_generate == &other.profile_generate
            && color_mode == &other.color_mode
            && suppress_rewrite_includes_only == &other.suppress_rewrite_includes_only
            && too_hard_for_preprocessor_cache_mode == &other.too_hard_for_preprocessor_cache_mode
            && out_dir == &other.out_dir
    }
}

impl ParsedArguments {
    pub fn output_pretty(&self) -> Cow<'_, str> {
        self.outputs
            .get("obj")
            .map(|o| o.path.as_os_str())
            .and_then(|s| s.to_str().map(Cow::Borrowed))
            .unwrap_or(Cow::Borrowed("Unknown filename"))
    }
}

/// A generic implementation of the `Compilation` trait for C/C++ compilers.
#[derive(Clone, Debug)]
struct CCompilation<T: CommandCreatorSync, I: CCompilerImpl> {
    service: SccacheService<T>,
    creator: T,
    parsed_args: ParsedArguments,
    is_locally_preprocessed: bool,
    executable: PathBuf,
    compiler: I,
    cwd: PathBuf,
    env_vars: Vec<(OsString, OsString)>,
    rewrite_includes_only: bool,
}

/// Supported C compilers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CCompilerKind {
    /// GCC
    Gcc,
    /// clang
    Clang,
    /// Diab
    Diab,
    /// Microsoft Visual C++
    Msvc,
    /// NVIDIA CUDA compiler
    Nvcc,
    /// NVIDIA CUDA front-end
    CudaFE,
    /// NVIDIA CUDA optimizer and PTX generator
    Cicc,
    /// NVIDIA CUDA PTX assembler
    Ptxas,
    /// NVIDIA hpc c, c++ compiler
    Nvhpc,
    /// Tasking VX
    TaskingVX,
    /// NVIDIA CUDA Tile IR assembler
    Tileiras,
}

impl From<&str> for CCompilerKind {
    fn from(kind: &str) -> Self {
        match kind {
            "clang" | "clang++" | "clang-cl" => CCompilerKind::Clang,
            "diab" => CCompilerKind::Diab,
            "gcc" | "g++" => CCompilerKind::Gcc,
            "cl" => CCompilerKind::Msvc,
            "nvcc" => CCompilerKind::Nvcc,
            "cudafe++" => CCompilerKind::CudaFE,
            "cicc" => CCompilerKind::Cicc,
            "ptxas" => CCompilerKind::Ptxas,
            "nvc" | "nvc++" => CCompilerKind::Nvhpc,
            "taskingvx" => CCompilerKind::TaskingVX,
            _ => unreachable!(),
        }
    }
}

impl From<CCompilerKind> for CompilerKind {
    fn from(kind: CCompilerKind) -> Self {
        CompilerKind::C(kind)
    }
}

impl From<&CCompilerKind> for CompilerKind {
    fn from(kind: &CCompilerKind) -> Self {
        CompilerKind::C(*kind)
    }
}

pub type ProcessOutputStream = dyn futures::stream::Stream<Item = Result<BytesMut>> + Send;
pub type DependenciesFuture = dyn futures::Future<Output = Result<Vec<PathBuf>>> + Send;

pub enum PreprocessorOutput {
    // cudafe++, cicc, and ptxas return this. Their output is always
    // saved to an intermediate file, and no preprocessor caching should
    // be done on them.
    File(PathBuf, Option<Pin<Box<DependenciesFuture>>>),
    Output(
        Pin<Box<ProcessOutputStream>>,
        Option<Pin<Box<DependenciesFuture>>>,
    ),
}

impl From<ProcessOutput> for Pin<Box<ProcessOutputStream>> {
    fn from(mut res: ProcessOutput) -> Self {
        if !res.success() {
            // Drop the stdout since it's the preprocessor output,
            // just hand back stderr and the exit status.
            res.stdout = vec![];
            Box::pin(futures::stream::iter([Err(ProcessError(res).into())]))
        } else {
            Box::pin(futures::stream::iter([Ok(BytesMut::from(Bytes::from(
                res.stdout,
            )))]))
        }
    }
}

pub enum DepfilePath {
    Path(PathBuf),
    Temp(TempPath),
}

impl DepfilePath {
    pub fn as_path(&self) -> &Path {
        match self {
            Self::Path(path) => path.as_path(),
            Self::Temp(temp) => temp.as_ref(),
        }
    }
}

impl fmt::Debug for DepfilePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_path())
    }
}

impl fmt::Display for DepfilePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_path().display())
    }
}

impl std::ops::Deref for DepfilePath {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.as_path()
    }
}

impl AsRef<Path> for DepfilePath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl AsRef<OsStr> for DepfilePath {
    fn as_ref(&self) -> &OsStr {
        self.as_path().as_os_str()
    }
}

/// An interface to a specific C compiler.
#[async_trait]
pub trait CCompilerImpl: Clone + fmt::Debug + Send + Sync + 'static {
    /// Return the kind of compiler.
    fn kind(&self) -> CCompilerKind;
    /// Return true iff this is g++ or clang++.
    fn plusplus(&self) -> bool;
    /// Return the compiler version reported by the compiler executable.
    fn version(&self) -> Option<String>;
    /// Determine whether `arguments` are supported by this compiler.
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<ParsedArguments>;
    /// Run the C preprocessor with the specified set of arguments.
    #[allow(clippy::too_many_arguments)]
    async fn preprocess<T>(
        &self,
        service: &SccacheService<T>,
        creator: &T,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        rewrite_includes_only: bool,
        generate_dependencies: bool,
        include_line_numbers: bool,
    ) -> Result<PreprocessorOutput>
    where
        T: CommandCreatorSync;

    /// Run the C preprocessor to generate the dependencies file.
    async fn generate_dependencies<T>(
        &self,
        creator: &T,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> Result<Option<DepfilePath>>
    where
        T: CommandCreatorSync;

    /// Generate a command that can be used to invoke the C compiler to perform
    /// the compilation.
    #[allow(clippy::too_many_arguments)]
    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        rewrite_includes_only: bool,
        hash_key: &str,
    ) -> Result<(
        impl CompileCommandImpl,
        Option<dist::CompileCommand>,
        Cacheable,
    )>;
}

impl<I> CCompiler<I>
where
    I: CCompilerImpl,
{
    pub async fn new(
        compiler: I,
        executable: PathBuf,
        extra_hash_files: Vec<PathBuf>,
    ) -> Result<CCompiler<I>> {
        trace!(
            "[CCompiler::new]: compiler={compiler:?}, executable={executable:?}, extra_hash_files={extra_hash_files:?}"
        );

        let paths = std::iter::once(executable.as_path())
            .chain(extra_hash_files.iter().map(|p| p.as_path()))
            .filter(|path| path.exists())
            .map(Ok);

        let executable_digest = futures::stream::iter(paths)
            .try_fold(Digest::new(), |digest, path| digest.with_file(path))
            .await
            .map(|mut digest| {
                if let Some(version) = compiler.version() {
                    digest.update(version.as_bytes());
                }
                digest.finish()
            })?;

        Ok(CCompiler {
            compiler,
            executable,
            executable_digest,
            preprocessor_dependencies_cache: Arc::new(Mutex::new(LruCache::new(
                // TODO: capacity chosen arbitrarily, should measure instead
                10_000,
            ))),
        })
    }

    fn extract_rocm_arg(args: &ParsedArguments, flag: &str) -> Option<PathBuf> {
        args.common_args.iter().find_map(|arg| match arg.to_str() {
            Some(sarg) if sarg.starts_with(flag) => {
                Some(PathBuf::from(sarg[arg.len()..].to_string()))
            }
            _ => None,
        })
    }

    fn extract_rocm_env(env_vars: &[(OsString, OsString)], name: &str) -> Option<PathBuf> {
        env_vars.iter().find_map(|(k, v)| match v.to_str() {
            Some(path) if k == name => Some(PathBuf::from(path.to_string())),
            _ => None,
        })
    }

    // See https://clang.llvm.org/docs/HIPSupport.html for details regarding the
    // order in which the environment variables and command-line arguments control the
    // directory to search for bitcode libraries.
    fn search_hip_device_libs(
        args: &ParsedArguments,
        env_vars: &[(OsString, OsString)],
    ) -> Vec<PathBuf> {
        let rocm_path_arg: Option<PathBuf> = Self::extract_rocm_arg(args, "--rocm-path=");
        let hip_device_lib_path_arg: Option<PathBuf> =
            Self::extract_rocm_arg(args, "--hip-device-lib-path=");
        let rocm_path_env: Option<PathBuf> = Self::extract_rocm_env(env_vars, "ROCM_PATH");
        let hip_device_lib_path_env: Option<PathBuf> =
            Self::extract_rocm_env(env_vars, "HIP_DEVICE_LIB_PATH");

        let hip_device_lib_path: PathBuf = hip_device_lib_path_arg
            .or(hip_device_lib_path_env)
            .or(rocm_path_arg.map(|path| path.join("amdgcn").join("bitcode")))
            .or(rocm_path_env.map(|path| path.join("amdgcn").join("bitcode")))
            // This is the default location in official AMD packages and containers.
            .unwrap_or(PathBuf::from("/opt/rocm/amdgcn/bitcode"));

        hip_device_lib_path
            .read_dir()
            .ok()
            .map(|f| {
                let mut device_libs = f
                    .flatten()
                    .filter(|f| f.path().extension().is_some_and(|ext| ext == "bc"))
                    .map(|f| f.path())
                    .collect::<Vec<_>>();
                device_libs.sort_unstable();
                device_libs
            })
            .unwrap_or_default()
    }

    pub fn compiler(&self) -> &I {
        &self.compiler
    }
}

impl<T: CommandCreatorSync, I: CCompilerImpl> Compiler<T> for CCompiler<I> {
    fn kind(&self) -> CompilerKind {
        CompilerKind::C(self.compiler.kind())
    }
    fn version(&self) -> Option<String> {
        self.compiler.version()
    }
    #[cfg(feature = "dist-client")]
    fn get_toolchain_packager(&self) -> Box<dyn pkg::ToolchainPackager> {
        Box::new(CToolchainPackager {
            env_vars: vec![],
            executable: self.executable.clone(),
            kind: self.compiler.kind(),
            parsed_args: Default::default(),
        })
    }
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<Box<dyn CompilerHasher<T> + 'static>> {
        match self.compiler.parse_arguments(arguments, cwd, env_vars) {
            CompilerArguments::Ok(mut args) => {
                // Handle SCCACHE_EXTRAFILES
                for (k, v) in env_vars.iter() {
                    if k.as_os_str() == OsStr::new("SCCACHE_EXTRAFILES") {
                        args.extra_hash_files.extend(std::env::split_paths(&v));
                    }
                }

                // Handle cache invalidation for the ROCm device bitcode libraries. Every HIP
                // object links in some LLVM bitcode libraries (.bc files), so in some sense
                // every HIP object compilation has an direct dependency on those bitcode
                // libraries.
                //
                // The bitcode libraries are unlikely to change **except** when a ROCm version
                // changes, so for correctness we should take these bitcode libraries into
                // account by adding them to `extra_hash_files`.
                //
                // In reality, not every available bitcode library is needed, but that is
                // too much to handle on our side so we just hash every bitcode library we find.
                if args.language == Language::Hip {
                    args.extra_hash_files
                        .extend(Self::search_hip_device_libs(&args, env_vars));
                }

                CompilerArguments::Ok(Box::new(CCompilerHasher {
                    parsed_args: args,
                    executable: self.executable.clone(),
                    executable_digest: self.executable_digest.clone(),
                    compiler: self.compiler.clone(),
                    preprocessor_dependencies_cache: self.preprocessor_dependencies_cache.clone(),
                }))
            }
            CompilerArguments::CannotCache(why, extra_info) => {
                CompilerArguments::CannotCache(why, extra_info)
            }
            CompilerArguments::NotCompilation => CompilerArguments::NotCompilation,
        }
    }

    fn box_clone(&self) -> Box<dyn Compiler<T>> {
        Box::new((*self).clone())
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any + Send + Sync> {
        self
    }
}

enum PreprocessorCacheLookup {
    Disabled,
    Hit(String),
    Miss(String),
}

impl<I> CCompilerHasher<I>
where
    I: CCompilerImpl,
{
    async fn preprocessor_cache_lookup(
        &self,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        extra_hashes: &[String],
        cache_control: &CacheControl,
        storage: &dyn Storage,
    ) -> Result<PreprocessorCacheLookup> {
        let CCompilerHasher {
            parsed_args,
            executable_digest,
            compiler,
            preprocessor_dependencies_cache,
            ..
        } = self;

        let out_pretty = parsed_args.output_pretty();
        let preprocessor_cache_mode_config = storage.preprocessor_cache_mode_config();

        if !use_preprocessor_cache_mode(parsed_args, env_vars, &preprocessor_cache_mode_config) {
            return Ok(PreprocessorCacheLookup::Disabled);
        }

        // Create an argument vector containing all the preprocessor args to use in creating a hash key
        let mut preprocessor_args = parsed_args.preprocessor_args.clone();
        // If the dependency args change, we need to re-run the preprocessor to generate them
        preprocessor_args.extend_from_slice(&parsed_args.dependency_args[..]);
        // common_args is used in preprocessing too
        preprocessor_args.extend_from_slice(&parsed_args.common_args[..]);
        preprocessor_args.extend_from_slice(&parsed_args.arch_args[..]);

        let preprocessor_key = preprocessor_cache_entry_hash_key(
            executable_digest,
            parsed_args.language,
            &preprocessor_args,
            extra_hashes,
            env_vars,
            cwd,
            &parsed_args.input,
            compiler.plusplus(),
            storage.basedirs(),
        )
        .await?
        .filter(|_| !matches!(cache_control, CacheControl::ForceNoCache));

        if let Some(preprocessor_key) = preprocessor_key {
            if matches!(cache_control, CacheControl::ForceRecache) {
                debug!("[{out_pretty}]: Preprocessor forced re-cache: {preprocessor_key}");
                return Ok(PreprocessorCacheLookup::Miss(preprocessor_key));
            }

            let preprocessor_cache_entry = PreprocessorCacheEntry::get(storage, &preprocessor_key)
                .await
                .inspect_err(|err| {
                    debug!("[{out_pretty}]: Error loading preprocessor cache entry for {preprocessor_key:?}: {err:#}");
                });

            if let Ok(Cache::Hit(mut preprocessor_cache_entry)) = preprocessor_cache_entry {
                let (updated, hit) = preprocessor_cache_entry
                    .lookup_result_digest(preprocessor_dependencies_cache.as_ref(), env_vars)
                    .await;

                let mut update_failed = false;
                if updated {
                    // Time macros have been found, we need to update the preprocessor cache entry.
                    // See [`PreprocessorCacheEntry::result_matches`].
                    debug!(
                        "[{out_pretty}]: Preprocessor cache updated because of time macros: {preprocessor_key}"
                    );

                    if let Err(e) = preprocessor_cache_entry
                        .put(storage, &preprocessor_key)
                        .await
                    {
                        debug!("[{out_pretty}]: Failed to update preprocessor cache: {e}");
                        update_failed = true;
                    }
                }

                if !update_failed && let Some(key) = hit {
                    debug!("[{out_pretty}]: Preprocessor cache hit: {preprocessor_key} -> {key}");
                    return Ok(PreprocessorCacheLookup::Hit(key));
                }
            }
            debug!("[{out_pretty}]: Preprocessor cache miss: {preprocessor_key}");
            return Ok(PreprocessorCacheLookup::Miss(preprocessor_key));
        }

        Ok(PreprocessorCacheLookup::Disabled)
    }
}

#[async_trait]
impl<T, I> CompilerHasher<T> for CCompilerHasher<I>
where
    T: CommandCreatorSync,
    I: CCompilerImpl,
{
    fn get_executable(&self) -> &Path {
        self.executable.as_path()
    }

    async fn generate_hash_key(
        self: Box<Self>,
        service: &SccacheService<T>,
        creator: &T,
        cwd: PathBuf,
        env_vars: Vec<(OsString, OsString)>,
        _pool: &tokio::runtime::Handle,
        rewrite_includes_only: bool,
        storage: Arc<dyn Storage>,
        cache_control: CacheControl,
    ) -> Result<HashResult<T>> {
        let start_of_compilation = std::time::SystemTime::now();

        let CCompilerHasher {
            compiler,
            executable,
            executable_digest,
            parsed_args,
            ..
        } = self.as_ref();

        let basedirs = storage.basedirs();

        // A compiler binary may be a symlink to another and so has the same digest, but that means
        // the toolchain will not contain the correct path to invoke the compiler! Add the compiler
        // executable path to try and prevent this
        let weak_toolchain_key = format!("{}-{}", executable.to_string_lossy(), executable_digest);

        // Skip preprocessing if we're not going to do any cache reads/writes
        // In this mode, the cache key doesn't matter, so return empty string
        if CacheControl::ForceNoCache == cache_control {
            return Ok(HashResult {
                key: String::new(),
                compilation: Box::new(CCompilation {
                    compiler: self.compiler,
                    creator: creator.to_owned(),
                    cwd,
                    env_vars,
                    executable: self.executable,
                    is_locally_preprocessed: false,
                    parsed_args: self.parsed_args,
                    rewrite_includes_only,
                    service: service.to_owned(),
                }),
                weak_toolchain_key,
            });
        }

        let extra_hashes = if parsed_args.extra_hash_files.is_empty() {
            vec![]
        } else {
            hash_all(&parsed_args.extra_hash_files).await?
        };

        let kind = compiler.kind().into();
        let lang = <CCompilerHasher<I> as CompilerHasher<T>>::language(&*self);

        // Try to look for a cached preprocessing step for this compilation request.
        let (preprocessor_cache_lookup, preprocessor_cache_lookup_duration) = {
            let start = Instant::now();
            let _pending = service.increment_pending_cache_lookups();
            let res = self
                .preprocessor_cache_lookup(
                    &cwd,
                    &env_vars,
                    &extra_hashes,
                    &cache_control,
                    storage.as_ref(),
                )
                .await?;
            (res, start.elapsed())
        };

        match preprocessor_cache_lookup {
            PreprocessorCacheLookup::Hit(key) => {
                let mut stats = service.stats.lock().await;
                stats.preprocessor_cache_hits.increment(&kind, &lang);
                stats.preprocessor_cache_hit_duration += preprocessor_cache_lookup_duration;

                // Skip preprocessing if it's a preprocessor cache hit
                return Ok(HashResult {
                    key,
                    compilation: Box::new(CCompilation {
                        compiler: self.compiler,
                        creator: creator.to_owned(),
                        cwd,
                        env_vars,
                        executable: self.executable,
                        is_locally_preprocessed: false,
                        parsed_args: self.parsed_args,
                        rewrite_includes_only,
                        service: service.to_owned(),
                    }),
                    weak_toolchain_key,
                });
            }
            PreprocessorCacheLookup::Miss(_) => {
                let mut stats = service.stats.lock().await;
                stats.preprocessor_cache_misses.increment(&kind, &lang);
                stats.preprocessor_cache_miss_duration += preprocessor_cache_lookup_duration;
            }
            _ => {}
        }

        let preprocessor = Preprocess::new(creator, service, compiler, parsed_args)
            .with_cwd(&cwd)
            .with_exe(executable)
            .with_env_vars(&env_vars)
            .with_basedirs(basedirs)
            .with_extra_hashes(&extra_hashes)
            .with_executable_digest(executable_digest)
            .rewrite_includes_only(rewrite_includes_only)
            // Generate dependencies if we're going to read them below
            .generate_dependencies(!matches!(
                preprocessor_cache_lookup,
                PreprocessorCacheLookup::Disabled
            ))
            // Include line numbers when `-fprofile-generate` is enabled
            // to guarantee the profile data embedded in the cached object
            // matches the line numbers in this source file
            .include_line_numbers(parsed_args.profile_generate);

        let (key, dependencies) = preprocessor
            .preprocess()
            .and_then(|(preprocessor_output, dependencies)| {
                preprocessor
                    .compute_hash_key(preprocessor_output)
                    .map_ok(|key| (key, dependencies))
            })
            .or_else(|err| preprocessor.clean_up(err))
            .await?;

        let preprocessor_key_and_dependencies = dependencies.and_then(|dependencies| {
            if let PreprocessorCacheLookup::Miss(preprocessor_key) = preprocessor_cache_lookup {
                Some((preprocessor_key, dependencies))
            } else {
                None
            }
        });

        if let Some((preprocessor_key, dependencies)) = preprocessor_key_and_dependencies {
            dependencies
                .map_ok(|dependencies| {
                    // Dedupe dependencies up front to ensure we only hash each file once
                    futures::stream::iter(
                        dependencies
                            .into_iter()
                            .sorted_unstable_by(|a, b| a.cmp(b))
                            .dedup()
                            .map(Ok),
                    )
                })
                .try_flatten_stream()
                .map_ok(|path| {
                    futures::stream::once(Box::pin(async {
                        let (digest, finder) =
                            Digest::from_file_with_time_macros(&path, &env_vars).await?;
                        if finder.found_time() {
                            // Write an entry for this dependency, even though it has __TIME__ macros.
                            // If it's still present next time, preprocessor cache mode will be disabled.
                            // If it's not present next time, the new object key will be added to this cache entry.
                            debug!("Found __TIME__ in {path:?}");
                        }
                        let meta = tokio::fs::symlink_metadata(&path).await?;
                        if include_is_too_new(&path, &(&meta).into(), start_of_compilation) {
                            bail!("Dependency changed after preprocessor invoked: {path:?}");
                        }
                        Ok((digest.finish(), path, meta))
                    }))
                })
                .try_flatten_unordered(None)
                .try_collect::<Vec<_>>()
                .and_then(|dependencies| async {
                    // Load the latest cache entry for this preprocessor key
                    // This helps minimize races if other clients write more
                    // entries while this client is preprocessing.
                    let mut preprocessor_cache_entry =
                        if let Ok(Cache::Hit(preprocessor_cache_entry)) =
                            PreprocessorCacheEntry::get(storage.as_ref(), &preprocessor_key).await
                        {
                            preprocessor_cache_entry
                        } else {
                            Default::default()
                        };

                    // Add the object key to the preprocessor cache entry
                    preprocessor_cache_entry.add_result(&preprocessor_key, &key, dependencies);

                    // Write the cache entry back to the preprocessor cache
                    preprocessor_cache_entry
                        .put(storage.as_ref(), &preprocessor_key)
                        .await
                })
                .await
                // Don't fail if updating the preprocessor cache entry fails, just log it
                .inspect_err(|err| {
                    debug!(
                        "[{}]: Failed to update preprocessor cache entry: {err}",
                        parsed_args.output_pretty()
                    );
                })
                .ok();
        }

        Ok(HashResult {
            key,
            compilation: Box::new(CCompilation {
                compiler: self.compiler,
                creator: creator.to_owned(),
                cwd,
                env_vars,
                executable: self.executable,
                is_locally_preprocessed: true,
                parsed_args: self.parsed_args,
                rewrite_includes_only,
                service: service.to_owned(),
            }),
            weak_toolchain_key,
        })
    }

    fn color_mode(&self) -> ColorMode {
        self.parsed_args.color_mode
    }

    fn output_pretty(&self) -> Cow<'_, str> {
        self.parsed_args.output_pretty()
    }

    fn box_clone(&self) -> Box<dyn CompilerHasher<T>> {
        Box::new((*self).clone())
    }

    fn language(&self) -> Language {
        self.parsed_args.language
    }
}

fn use_preprocessor_cache_mode(
    parsed_args: &ParsedArguments,
    env_vars: &[(OsString, OsString)],
    preprocessor_cache_mode_config: &PreprocessorCacheModeConfig,
) -> bool {
    let out_pretty = parsed_args.output_pretty();

    if !parsed_args.language.needs_c_preprocessing() {
        trace!(
            "[{out_pretty}]: Disabling preprocessor cache because {} language doesn't need C preprocessing",
            parsed_args.language.as_str()
        );
        return false;
    }

    // Try to look for a cached preprocessing step for this compilation
    // request.

    if !parsed_args.too_hard_for_preprocessor_cache_mode.is_empty() {
        trace!(
            "[{out_pretty}]: Cannot use preprocessor cache because {:?}",
            parsed_args
                .too_hard_for_preprocessor_cache_mode
                .join(OsStr::new(" "))
        );
    }

    let can_use_preprocessor_cache_mode = preprocessor_cache_mode_config
        .use_preprocessor_cache_mode
        && parsed_args.too_hard_for_preprocessor_cache_mode.is_empty();

    // Allow overrides from the env
    let use_preprocessor_cache_mode = env_vars
        .iter()
        .find_map(|(k, v)| {
            if k == "SCCACHE_DIRECT" {
                v.to_str().map(|s| s.to_lowercase())
            } else {
                None
            }
        })
        .map(|v| !matches!(v.as_str(), "false" | "off" | "0" | ""))
        .unwrap_or(can_use_preprocessor_cache_mode);

    if can_use_preprocessor_cache_mode && !use_preprocessor_cache_mode {
        trace!("[{out_pretty}]: Disabling preprocessor cache because SCCACHE_DIRECT=false");
    }

    use_preprocessor_cache_mode
}

struct Preprocess<'a, T: Send, I> {
    basedirs: &'a [Vec<u8>],
    compiler: &'a I,
    creator: &'a T,
    cwd: &'a Path,
    env_vars: &'a [(OsString, OsString)],
    exe: &'a Path,
    executable_digest: &'a str,
    extra_hashes: &'a [String],
    generate_dependencies: bool,
    include_line_numbers: bool,
    out_pretty: Cow<'a, str>,
    parsed_args: &'a ParsedArguments,
    rewrite_includes_only: bool,
    service: &'a SccacheService<T>,
}

impl<'a, T, I> Preprocess<'a, T, I>
where
    T: CommandCreatorSync,
    I: CCompilerImpl,
{
    fn new(
        creator: &'a T,
        service: &'a SccacheService<T>,
        compiler: &'a I,
        parsed_args: &'a ParsedArguments,
    ) -> Self {
        Self {
            creator,
            service,
            compiler,
            parsed_args,
            out_pretty: parsed_args.output_pretty(),
            cwd: Path::new(""),
            exe: Path::new(""),
            env_vars: &[],
            rewrite_includes_only: false,
            generate_dependencies: false,
            include_line_numbers: false,
            executable_digest: "",
            extra_hashes: &[],
            basedirs: &[],
        }
    }

    /// Sets the base directories for path normalization.
    fn with_basedirs(mut self, basedirs: &'a [Vec<u8>]) -> Self {
        self.basedirs = basedirs;
        self
    }

    fn with_cwd(mut self, cwd: &'a Path) -> Self {
        self.cwd = cwd;
        self
    }

    /// Sets the environment variables to consider for caching.
    fn with_env_vars(mut self, env_vars: &'a [(OsString, OsString)]) -> Self {
        self.env_vars = env_vars;
        self
    }

    fn with_exe(mut self, exe: &'a Path) -> Self {
        self.exe = exe;
        self
    }

    fn with_executable_digest(mut self, executable_digest: &'a str) -> Self {
        self.executable_digest = executable_digest;
        self
    }

    /// Sets additional hash data to include in the cache key.
    fn with_extra_hashes(mut self, extra_hashes: &'a [String]) -> Self {
        self.extra_hashes = extra_hashes;
        self
    }

    fn generate_dependencies(mut self, generate_dependencies: bool) -> Self {
        self.generate_dependencies = generate_dependencies;
        self
    }

    fn include_line_numbers(mut self, include_line_numbers: bool) -> Self {
        self.include_line_numbers = include_line_numbers;
        self
    }

    fn rewrite_includes_only(mut self, rewrite_includes_only: bool) -> Self {
        self.rewrite_includes_only = rewrite_includes_only;
        self
    }

    async fn preprocess(
        &self,
    ) -> Result<(
        Pin<Box<ProcessOutputStream>>,
        Option<Pin<Box<DependenciesFuture>>>,
    )> {
        let Self {
            creator,
            service,
            compiler,
            parsed_args,
            cwd,
            exe,
            env_vars,
            rewrite_includes_only,
            generate_dependencies,
            include_line_numbers,
            ..
        } = *self;

        let needs_c_preprocessing = parsed_args.language.needs_c_preprocessing();

        let preprocessor_output = if !needs_c_preprocessing {
            PreprocessorOutput::File(cwd.join(&parsed_args.input), None)
        } else {
            compiler
                .preprocess(
                    service,
                    creator,
                    exe,
                    parsed_args,
                    cwd,
                    env_vars,
                    rewrite_includes_only,
                    generate_dependencies,
                    include_line_numbers,
                )
                .await?
        };

        let (mut preprocessor_output, dependencies) = match preprocessor_output {
            PreprocessorOutput::File(path, dependencies) => {
                let src = tokio::fs::File::open(path).await?;
                let src = read_line_batches(src, HASH_BUFFER_SIZE);
                (Box::pin(src) as Pin<Box<ProcessOutputStream>>, dependencies)
            }
            PreprocessorOutput::Output(preprocessor_output, dependencies) => {
                (preprocessor_output, dependencies)
            }
        };

        let (preprocessor_output, dependencies) = if !needs_c_preprocessing {
            (preprocessor_output, dependencies)
        } else {
            let cwd = cwd.to_path_buf();
            let service = service.clone();
            let has_dependencies = dependencies.is_some();
            let (dependencies_tx, dependencies_rx) = tokio::sync::oneshot::channel();

            let preprocessor_output = Box::pin(async_stream::try_stream! {
                let mut preprocessor_time = Duration::default();

                loop {
                    let preprocessor_start = Instant::now();
                    let lines = preprocessor_output.next().await;
                    preprocessor_time += preprocessor_start.elapsed();
                    if let Some(lines) = lines {
                        yield lines?;
                    } else {
                        break;
                    }
                }

                // Track how long the preprocessor took
                let mut stats = service.stats.lock().await;
                stats.preprocessed += 1;
                stats.preprocessor_duration += preprocessor_time;

                if let Some(dependencies) = dependencies {
                    // Canonicalize the dependency paths in a background thread
                    let dependencies = dependencies.and_then(|dependencies| async move {
                        tokio::task::spawn_blocking(move || {
                            dependencies
                                .into_iter()
                                // Each dependency in a preprocessor cache entry must exist,
                                // and we must use the absolute path to the dependency.
                                .map(|path| cwd.join(path))
                                .map(|path| dunce::canonicalize(&path).with_context(|| format!("{path:?}")))
                                .try_collect::<_, Vec<_>, _>()
                        })
                        .await?
                    })
                    .await;

                    let _ = dependencies_tx.send(dependencies);
                }
            }) as Pin<Box<ProcessOutputStream>>;

            let dependencies = if has_dependencies {
                Some(Box::pin(async {
                    dependencies_rx
                        .await
                        .unwrap_or_else(|_| bail!("channel error"))
                }) as Pin<Box<DependenciesFuture>>)
            } else {
                None
            };

            (preprocessor_output, dependencies)
        };

        Ok((preprocessor_output, dependencies))
    }

    async fn compute_hash_key(
        &self,
        preprocessor_output: Pin<Box<ProcessOutputStream>>,
    ) -> Result<String> {
        // Create an argument vector containing both common and arch args
        let common_and_arch_args = [
            &self.parsed_args.common_args[..],
            &self.parsed_args.arch_args[..],
        ]
        .concat();

        HashKeyParams::new(
            self.executable_digest,
            self.parsed_args.language,
            &common_and_arch_args,
            preprocessor_output,
        )
        .with_extra_hashes(self.extra_hashes)
        .with_env_vars(self.env_vars)
        .with_plusplus(self.compiler.plusplus())
        .with_basedirs(self.basedirs)
        .compute()
        .await
    }

    async fn clean_up<R, E: Into<anyhow::Error>>(
        &self,
        err: E,
    ) -> std::result::Result<R, anyhow::Error> {
        let Self {
            cwd,
            out_pretty,
            parsed_args,
            ..
        } = self;

        let outputs = &parsed_args.outputs;
        // Errors remove all traces of potential output.
        trace!("[{out_pretty}]: removing files {outputs:?}");

        // Remove any files written by failed preprocessor call
        let v = futures::stream::iter(outputs.values())
            .map(Ok)
            .try_filter_map(|output| async {
                let path = cwd.join(&output.path);
                if tokio::fs::metadata(&path).await.is_ok() {
                    Ok(Some(path))
                } else {
                    Ok(None)
                }
            })
            .try_for_each_concurrent(None, tokio::fs::remove_file)
            .await;

        if v.is_err() {
            warn!("[{out_pretty}]: Could not remove files after preprocessing failed: {outputs:?}");
        }

        match err.into().downcast::<ProcessError>() {
            Ok(ProcessError(mut output)) => {
                debug!(
                    "[{out_pretty}]: preprocessor returned error (code={:?}, desc={:?})",
                    output.code(),
                    output.desc()
                );
                // Drop the stdout since it's the preprocessor output,
                // just hand back stderr and the exit status.
                output.stdout = vec![];
                Err(ProcessError(output).into())
            }
            Err(err) => {
                warn!("[{out_pretty}]: preprocessor failed: {err:?}");
                Err(err)
            }
        }
    }
}

#[derive(Debug, Clone)]
struct CCompilerCommand<C, I, T>
where
    C: CCompilerImpl,
    I: CompileCommandImpl,
    T: CommandCreatorSync,
{
    cmd: I,
    compilation: CCompilation<T, C>,
}

impl<C, I, T> CCompilerCommand<C, I, T>
where
    C: CCompilerImpl,
    I: CompileCommandImpl,
    T: CommandCreatorSync,
{
    #[allow(clippy::new_ret_no_self)]
    pub fn new(cmd: I, compilation: CCompilation<T, C>) -> Box<dyn CompileCommand<T>> {
        Box::new(CCompilerCommand { cmd, compilation }) as Box<dyn CompileCommand<T>>
    }
}

#[async_trait]
impl<C, I, T> CompileCommand<T> for CCompilerCommand<C, I, T>
where
    C: CCompilerImpl,
    I: CompileCommandImpl,
    T: CommandCreatorSync,
{
    fn get_executable(&self) -> PathBuf {
        self.cmd.get_executable()
    }
    fn get_arguments(&self) -> Vec<OsString> {
        self.cmd.get_arguments()
    }
    fn get_env_vars(&self) -> Vec<(OsString, OsString)> {
        self.cmd.get_env_vars()
    }
    fn get_cwd(&self) -> PathBuf {
        self.cmd.get_cwd()
    }

    async fn execute(&self, service: &SccacheService<T>, creator: &T) -> Result<ProcessOutput> {
        let out = self.cmd.execute(service, creator).await?;
        // Ensure the dependency file exists
        self.compilation.generate_dependencies(creator).await?;
        Ok(out)
    }

    fn box_clone(&self) -> Box<dyn CompileCommand<T>> {
        CCompilerCommand::<C, I, T>::new(self.cmd.clone(), self.compilation.clone())
    }
}

#[async_trait]
impl<T: CommandCreatorSync, I: CCompilerImpl> Compilation<T> for CCompilation<T, I> {
    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
        rewrite_includes_only: bool,
        hash_key: &str,
    ) -> Result<(
        Box<dyn CompileCommand<T>>,
        Option<dist::CompileCommand>,
        Cacheable,
    )> {
        let CCompilation {
            parsed_args,
            executable,
            compiler,
            cwd,
            env_vars,
            ..
        } = self;

        compiler
            .generate_compile_commands(
                path_transformer,
                executable,
                parsed_args,
                cwd,
                env_vars,
                rewrite_includes_only,
                hash_key,
            )
            .map(|(command, dist_command, cacheable)| {
                (
                    CCompilerCommand::new(command, self.clone()),
                    dist_command,
                    cacheable,
                )
            })
    }

    #[allow(dead_code)]
    async fn generate_dependencies(&self, creator: &T) -> Result<()> {
        let CCompilation {
            parsed_args,
            executable,
            compiler,
            cwd,
            env_vars,
            ..
        } = self;

        // Ensure the depfile exists if it is required and doesn't already.
        //
        // When not configured for dist-compile, the depfile is either created
        // by the preprocessor, generated during compile, or restored from
        // cache.
        //
        // If we're using preprocessor cache mode with sccache-dist, it's possible
        // to get a preprocessor cache hit, an object cache miss (i.e. changed from
        // remote to local caching), and then dist-compile. However, dist-compile
        // doesn't generate dependency files because it compiles the preprocessed
        // source. Preprocessor-cache mode means we skip calling the preprocessor,
        // so we have to generate the dependency file after the fact.
        if let Some(depfile) = parsed_args
            .depfile
            .as_ref()
            .map(|depfile| cwd.join(depfile))
            && !depfile.exists()
        {
            compiler
                .generate_dependencies(creator, executable, parsed_args, cwd, env_vars)
                .await?;
        }
        Ok(())
    }

    #[cfg(feature = "dist-client")]
    fn into_dist_packagers(self: Box<Self>) -> Result<DistPackagers> {
        trace!(
            "Dist inputs: {:?}",
            std::iter::once(&self.cwd.join(&self.parsed_args.input))
                .chain(self.parsed_args.extra_dist_files.iter())
                .chain(self.parsed_args.extra_hash_files.iter())
                .unique()
                .collect::<Vec<_>>()
        );

        let toolchain_packager = Box::new(CToolchainPackager {
            kind: self.compiler.kind(),
            env_vars: self.env_vars.clone(),
            executable: self.executable.clone(),
            parsed_args: self.parsed_args.clone(),
        });

        let outputs_rewriter = Box::new(NoopOutputsRewriter);

        Ok((self, toolchain_packager, outputs_rewriter))
    }

    fn is_locally_preprocessed(&self) -> bool {
        self.is_locally_preprocessed
    }

    fn outputs<'a>(&'a self) -> Box<dyn Iterator<Item = FileObjectSource> + 'a> {
        Box::new(self.parsed_args.outputs.iter().map(|(k, output)| {
            let dir = self
                .cwd
                .join(&output.path)
                .parent()
                .unwrap_or(self.cwd.as_path())
                .to_owned();
            FileObjectSource {
                key: k.to_string(),
                dir,
                path: output.path.clone(),
                optional: output.optional,
            }
        }))
    }
}

#[cfg(feature = "dist-client")]
#[async_trait]
impl<T: CommandCreatorSync, I: CCompilerImpl> pkg::InputsPackager for CCompilation<T, I> {
    async fn write_inputs(
        self: Box<Self>,
        path_transformer: &mut dist::PathTransformer,
        compressor: Box<dyn InputsWriter>,
    ) -> Result<()> {
        use std::collections::BTreeMap;

        let CCompilation {
            service,
            creator,
            cwd,
            executable,
            compiler,
            env_vars,
            mut parsed_args,
            rewrite_includes_only,
            ..
        } = *self;

        // Workaround for nvc++ 26.3:
        // Remove `--nvcchost` from the preprocessor call that creates
        // the dist input because `--nvcchost` defines `va_list` twice
        let mut orig_common_args = None;
        if matches!(compiler.kind(), CCompilerKind::Nvhpc) {
            orig_common_args.replace(parsed_args.common_args.clone());
            parsed_args.common_args.retain(|arg| arg != "--nvcchost");
        }

        let preprocessor = Preprocess::new(&creator, &service, &compiler, &parsed_args)
            .with_cwd(&cwd)
            .with_exe(&executable)
            .with_env_vars(&env_vars)
            .rewrite_includes_only(rewrite_includes_only)
            .generate_dependencies(true)
            .include_line_numbers(true);

        let (preprocessor_output, dependencies) = preprocessor
            .preprocess()
            .and_then(|(preprocessor_output, dependencies)| async {
                // Unfortunate that we have to buffer all the preprocessor output.
                // The flate2 compressors don't implement io::Seek, but that's a
                // requirement to use tar::Builder::append_writer.
                let preprocessor_output = preprocessor_output.try_concat().await?;
                let dependencies = if let Some(dependencies) = dependencies {
                    dependencies.await?
                } else {
                    vec![]
                };
                Ok((preprocessor_output, dependencies))
            })
            .or_else(|err| preprocessor.clean_up(err))
            .await?;

        // Restore common_args if we applied a workaround for nvc++ 26.3
        if let Some(common_args) = orig_common_args {
            parsed_args.common_args = common_args;
        }

        let mut symlinks = BTreeMap::new();
        let mut builder = tar::Builder::new(compressor);
        let mut path_transformer = path_transformer.clone();

        let (mut builder, mut symlinks, mut path_transformer) =
            tokio::task::spawn_blocking(move || {
                // Find symlinks and simplify the input path first
                let input_path = cwd.join(&parsed_args.input);
                let input_path = pkg::tarify_path(&mut symlinks, &input_path)?;
                let dist_path = if !parsed_args.language.needs_c_preprocessing() {
                    path_transformer.as_dist(&input_path)
                } else {
                    path_transformer.as_dist_input_path(&input_path)
                }
                .with_context(|| format!("unable to transform input path {input_path:?}"))?;

                let (mut header, dist_path) = pkg::make_tar_header(&input_path, &dist_path)?;
                // The current size is from the non-preprocessed path, so set the actual size.
                header.set_size(preprocessor_output.len() as u64);
                header.set_cksum();
                builder.append_data(&mut header, dist_path, &preprocessor_output[..])?;

                Ok::<_, anyhow::Error>((builder, symlinks, path_transformer))
            })
            .await??;

        // Add the dependencies and extra files
        let builder = tokio::task::spawn_blocking(move || {
            // Find and add symlinks to the tar archive before the files.
            // This ensures the symlinks are unpacked by the receiver before
            // files which may traverse them.
            let tarified_extra_paths = [
                &dependencies[..],
                &parsed_args.extra_dist_files[..],
                &parsed_args.extra_hash_files[..],
            ]
            .into_iter()
            .flatten()
            .map(|path| pkg::tarify_path(&mut symlinks, path))
            .try_collect::<_, Vec<_>, _>()?;

            for (from_path, to_path) in symlinks.iter() {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mtime(0);
                header.set_entry_type(tar::EntryType::Symlink);
                // Leave `to_path` as absolute, assuming the tar will
                // be used in a chroot-like environment.
                builder.append_link(&mut header, pkg::tar_safe_path(from_path), to_path)?;
            }

            for path in tarified_extra_paths {
                if !super::CAN_DIST_DYLIBS
                    && path
                        .extension()
                        .is_some_and(|ext| ext == std::env::consts::DLL_EXTENSION)
                {
                    bail!(
                        "Cannot distribute dylib input {} on this platform",
                        path.display()
                    )
                }

                builder.append_path_with_name(
                    &path,
                    path_transformer
                        .as_dist(&path)
                        .map(pkg::tar_safe_path)
                        .with_context(|| {
                            format!("unable to transform input path {}", path.display())
                        })?,
                )?;
            }

            Ok::<_, anyhow::Error>(builder)
        })
        .await??;

        // Finish archive
        let _ = builder.into_inner()?.finish()?;
        Ok(())
    }
}

#[cfg(feature = "dist-client")]
#[allow(unused)]
struct CToolchainPackager {
    env_vars: Vec<(OsString, OsString)>,
    executable: PathBuf,
    kind: CCompilerKind,
    parsed_args: ParsedArguments,
}

#[cfg(feature = "dist-client")]
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
))]
#[async_trait]
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "aarch64"),
))]
impl pkg::ToolchainPackager for CToolchainPackager {
    async fn package(&self) -> Result<Arc<dyn pkg::PackagedToolchain>> {
        use std::os::unix::ffi::OsStringExt;
        use tokio_util::compat::TokioAsyncReadCompatExt;

        debug!(
            "Packaging toolchain for executable {:?}",
            self.executable.display()
        );
        let mut package_builder = pkg::ToolchainPackaged::new(self.executable.clone());
        package_builder.add_common()?;

        // Helper to use -print-file-name and -print-prog-name to look up
        // files by path.
        let named_file = |arg: &str| -> Option<PathBuf> {
            let mut output = std::process::Command::new(&self.executable)
                .arg(arg)
                .output()
                .ok()?;
            debug!(
                "{} {arg} output:\n{}\n===\n{}",
                self.executable.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            if !output.status.success() {
                debug!("exit failure");
                return None;
            }

            // Remove the trailing newline (if present)
            if output.stdout.last() == Some(&b'\n') {
                output.stdout.pop();
            }

            // Create our PathBuf from the raw bytes.  Assume that relative
            // paths can be found via PATH.
            let path: PathBuf = OsString::from_vec(output.stdout).into();
            if path.is_absolute() {
                Some(path)
            } else {
                which::which(path).ok()
            }
        };

        // Helper to add a named file/program by to the package.
        // We ignore the case where the file doesn't exist, as we don't need it.
        let add_named_prog = |builder: &mut pkg::ToolchainPackaged, name: &str| -> Result<()> {
            if let Some(path) = named_file(&format!("-print-prog-name={name}")) {
                builder.add_executable_and_deps(&self.env_vars, &path)?;
            }
            Ok(())
        };
        let add_named_file = |builder: &mut pkg::ToolchainPackaged, name: &str| -> Result<()> {
            if let Some(path) = named_file(&format!("-print-file-name={name}")) {
                builder.add_file(&self.env_vars, path)?;
            }
            Ok(())
        };

        let mut add_default_files = || -> Result<()> {
            // Add basic |as| and |objcopy| programs.
            add_named_prog(&mut package_builder, "as")?;
            add_named_prog(&mut package_builder, "objcopy")?;

            // Linker configuration.
            if Path::new("/etc/ld.so.conf").is_file() {
                package_builder.add_file(&self.env_vars, "/etc/ld.so.conf".into())?;
            }
            let ld_conf_dir = Path::new("/etc/ld.so.conf.d");
            if ld_conf_dir.is_dir() {
                package_builder.add_dir_contents(&self.env_vars, ld_conf_dir)?;
            }
            Ok(())
        };

        // Compiler-specific handling
        match self.kind {
            CCompilerKind::Clang => {
                add_default_files()?;
                package_builder.add_executable_and_deps(&self.env_vars, &self.executable)?;
                // Clang uses internal header files, so add them.
                if let Some(limits_h) = named_file("-print-file-name=include/limits.h") {
                    info!("limits_h = {}", limits_h.display());
                    package_builder.add_dir_contents(&self.env_vars, limits_h.parent().unwrap())?;
                }
            }

            CCompilerKind::Gcc => {
                // Various external programs / files which may be needed by gcc
                add_default_files()?;
                package_builder.add_executable_and_deps(&self.env_vars, &self.executable)?;
                add_named_prog(&mut package_builder, "cc1")?;
                add_named_prog(&mut package_builder, "cc1plus")?;
                add_named_file(&mut package_builder, "liblto_plugin.so")?;
                // Add gcc implicit specfiles
                let jobserver = crate::jobserver::Client::new_num(1);
                let mut creator = crate::mock_command::ProcessCommandCreator::new(&jobserver);
                for path in crate::compiler::gcc::Gcc::read_implicit_specfiles(
                    &mut creator,
                    &self.executable,
                    &[],
                    &self.env_vars,
                    "-v",
                )
                .await?
                {
                    package_builder.add_file(&self.env_vars, path)?;
                }
            }

            CCompilerKind::Nvhpc => {
                // Various programs called by the nvc/nvc++ front end.
                let _ = add_default_files();

                // Handle NVHPC's mpicc/mpic++ compiler wrappers
                let executable = if let Some(executable) = named_file("-showme:command") {
                    package_builder.add_executable_and_deps(&self.env_vars, &executable)?;

                    // Handle NVHPC's symlinks of `mpic++ -> bin/env.sh` where `env.sh`
                    // is a wrapper to the real `mpic++` executable at `bin/.bin/mpic++`
                    if let Err(orig_err) =
                        package_builder.add_executable_and_deps(&self.env_vars, &self.executable)
                    {
                        if let Some(exe_dir) = self.executable.parent() {
                            let dot_bin_dir = if let Ok(path) = self.executable.read_link() {
                                exe_dir.join(path).parent().map(|p| p.join(".bin"))
                            } else {
                                Some(exe_dir).map(|p| p.join(".bin"))
                            };
                            if let Some(real_exe) = dot_bin_dir
                                .and_then(|p| self.executable.file_name().map(|name| p.join(name)))
                            {
                                if !real_exe.exists() {
                                    return Err(orig_err);
                                }
                                // Symlink `bin/mpic++` -> `bin/.bin/mpic++`
                                package_builder.add_link(&real_exe, &self.executable)?;
                                package_builder
                                    .add_executable_and_deps(&self.env_vars, &real_exe)?;
                            }
                        } else {
                            return Err(orig_err);
                        }
                    }
                    executable
                } else {
                    // If `-showme:command` fails, self.executable is nvc/nvc++
                    package_builder.add_executable_and_deps(&self.env_vars, &self.executable)?;
                    self.executable.clone()
                };

                let (contents, dirs, mut files): (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) = {
                    use futures::{AsyncBufReadExt, future, io::BufReader};
                    use std::process::Stdio;

                    let mut child = tokio::process::Command::new(&executable)
                        .arg("-show")
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()?;

                    let info = child.stdout.take().unwrap();
                    let info = BufReader::new(info.compat());

                    // DEFCPPINC      These dirs just need to exist
                    // DEFSTDINC      These dirs just need to exist
                    // GCCINC         These dirs just need to exist
                    // GPPDIR         These dirs just need to exist
                    // STDINC         These dirs just need to exist
                    // SYSTEMINC      These dirs just need to exist
                    // NVCOMPILER                                         =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7
                    // COMPBIN        Compiler binary directory           =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/bin
                    // COMPLIB        Compiler library directory          =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/lib
                    // CCOMPDIR       Directory containing the C compiler =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/bin/tools
                    // CPPCOMPDIR     Directory containing the C compiler =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/bin/tools
                    // COMPINCDIRFULL                                     =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/include /opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/include-stdexec
                    // CUDAROOT       (sometimes not set)                 =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/cuda/12.9
                    // DCUDADIR                                           =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/cuda/
                    // USECUDAROOT                                        =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/cuda/
                    // CUDADIR                                            =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/cuda/12.9/bin
                    // CUDALIBDIR                                         =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/cuda/12.9/lib64
                    // LLVMBINDIR     Directory containing LLVM tools     =/opt/nvidia/hpc_sdk/Linux_x86_64/25.7/compilers/share/llvm/bin
                    // DEFCUDAVERSIONVAL                                  =12.9
                    // DEFCUDAVERSION                                     =12.9

                    let lines = info
                        .lines()
                        .try_filter(|line| future::ready(line.len() >= 14 && line.contains('=')))
                        .try_filter_map(|line| {
                            future::ok(match &line[0..14] {
                                "CCOMPDIR      " | // .
                                "COMPBIN       " | // .
                                "COMPINCDIRFULL" | // .
                                "COMPLIB       " | // .
                                "CPPCOMPDIR    " | // .
                                "CUDADIR       " | // .
                                "CUDALIBDIR    " | // .
                                "CUDAROOT      " | // .
                                "DCUDADIR      " | // .
                                "DEFCPPINC     " | // .
                                "DEFSTDINC     " | // .
                                "GCCINC        " | // .
                                "GPPDIR        " | // .
                                "LLVMBINDIR    " | // .
                                "NVCOMPILER    " | // .
                                "STDINC        " | // .
                                "SYSTEMINC     " | // .
                                "USECUDAROOT   " | // .
                                "DEFCUDAVERSION" => {
                                    Some(line)
                                }
                                _ => None,
                            })
                        })
                        .try_collect::<Vec<_>>()
                        .await?;

                    // Search for the CUDA version first
                    let cuda_version = lines.iter().find_map(|line| match &line[0..14] {
                        "DEFCUDAVERSION" => {
                            line.split_once('=').map(|(_, ver)| ver.trim().to_owned())
                        }
                        _ => None,
                    });

                    lines
                        .into_iter()
                        .filter_map(|line| {
                            let name_and_dirs = line.split_once('=').map(|(name, dirs)| {
                                // `name` is the left-hand side of the '=', i.e. "NAME    Description of NAME "
                                //
                                //  There's always a space after NAME so it's safe to use `split_once(' ')`,
                                //  but I'm being defensive in case a future version of NVHPC emits lines
                                //  without any spaces between the name and value (like "NAME=/foo/bar")
                                //
                                // `dirs` is the right-hand side of the '=', i.e. "/foo/bar /foo/baz"
                                let (name, _) = name.split_once(' ').unwrap_or((name, ""));
                                (
                                    name.trim().to_owned(),
                                    dirs.trim()
                                        .split(' ')
                                        .map(|s| s.trim())
                                        .map(PathBuf::from)
                                        .collect::<Vec<_>>(),
                                )
                            });

                            name_and_dirs.and_then(|(name, dirs)| {
                                match &name[..] {
                                    "DCUDADIR" | "USECUDAROOT" => {
                                        // Append the CUDA version to these paths since they don't have them.
                                        // This is important if CUDAROOT isn't set.
                                        cuda_version.as_ref().map(|cuda_version| {
                                            (
                                                name,
                                                dirs.into_iter()
                                                    .flat_map(|dir| {
                                                        [dir.as_path().join(cuda_version), dir]
                                                    })
                                                    .collect::<Vec<_>>(),
                                            )
                                        })
                                    }
                                    _ => Some((name, dirs)),
                                }
                            })
                        })
                        .map(|(name, dirs)| {
                            let mut contents = vec![];
                            let mut files = vec![];

                            match &name[..] {
                                "NVCOMPILER" => {
                                    files.extend(dirs.iter().flat_map(|root| {
                                        use crate::util::OsStrExt;
                                        walkdir::WalkDir::new(root)
                                            .follow_links(true)
                                            .same_file_system(true)
                                            .into_iter()
                                            .filter_map_ok(|e| {
                                                if e.file_type().is_file() {
                                                    let name = e.file_name();
                                                    None
                                                        // rcfiles
                                                        .or(name.ends_with("rc").then_some(true))
                                                        .or(name.ends_with(".bc").then_some(true))
                                                        // mpic++-wrapper-data.txt
                                                        .or(name
                                                            .ends_with("-wrapper-data.txt")
                                                            .then_some(true))
                                                        .or(name
                                                            .ends_with("help-opal-wrapper.txt")
                                                            .then_some(true))
                                                        .map(|_| e.into_path())
                                                } else {
                                                    None
                                                }
                                            })
                                            .filter_map(|r| r.ok())
                                    }));
                                }
                                "COMPBIN" => {
                                    contents.extend(dirs.iter().map(|root| root.join("rcfiles")));
                                }
                                "COMPLIB" => {
                                    files.extend(dirs.iter().flat_map(|root| {
                                        [
                                            "acc_init_link_cuda.o",
                                            "cuda_init_register_end.o",
                                            "init_pgpf.o",
                                            "libmem.il",
                                            "nvhpc.ld",
                                            "nvhpc.syms",
                                        ]
                                        .iter()
                                        .map(|name| root.join(name))
                                    }));
                                }
                                "CCOMPDIR" | "CPPCOMPDIR" => {
                                    // Add everything under `compilers/bin/tools`
                                    contents.extend_from_slice(dirs.as_slice());
                                }
                                "COMPINCDIRFULL" => {
                                    // NVHPC uses internal LLVM header files, so add them.
                                    contents.extend_from_slice(dirs.as_slice());
                                }
                                "CUDAROOT" | "DCUDADIR" | "USECUDAROOT" => {
                                    files
                                        .extend(dirs.iter().map(|root| root.join("nvvm/bin/cicc")));
                                    files.extend(dirs.iter().flat_map(|root| {
                                        ["include/cuda.h", "nvvm/lib64/libnvvm.so"]
                                            .iter()
                                            .map(|name| root.join(name))
                                    }));
                                }
                                "CUDADIR" => {
                                    files.extend(dirs.iter().flat_map(|root| {
                                        [
                                            "bin2c",
                                            "cudafe++",
                                            "fatbinary",
                                            "nvlink",
                                            "nvprune",
                                            "ptxas",
                                        ]
                                        .iter()
                                        .map(|name| root.join(name))
                                    }));
                                    files.extend(dirs.iter().map(|root| root.join("nvcc.profile")));
                                }
                                "CUDALIBDIR" => {}
                                "LLVMBINDIR" => {
                                    files.extend(dirs.iter().flat_map(|root| {
                                        ["llc", "opt", "llvm-as", "llvm-link", "llvm-mc"]
                                            .iter()
                                            .map(|name| root.join(name))
                                    }));
                                }
                                _ => {}
                            }

                            (contents, dirs, files)
                        })
                        .fold((vec![], vec![], vec![]), |mut acc, res| {
                            acc.0.extend(res.0);
                            acc.1.extend(res.1);
                            acc.2.extend(res.2);
                            acc
                        })
                };

                if let Ok(as_path) = which::which("as") {
                    files.push(as_path);
                }

                for path in dirs.into_iter().filter(|p| p.exists()) {
                    if path.is_dir() {
                        let _ = package_builder
                            .add_dir(path.clone())
                            .map_err(|e| trace!("add_dir error {path:?}: {e:?}"));
                    }
                }

                for path in files.into_iter().filter(|p| p.exists()) {
                    if path.is_file() || path.is_symlink() {
                        let _ = package_builder
                            .add_file(&self.env_vars, path.clone())
                            .map_err(|e| trace!("add_file error {path:?}: {e:?}"));
                    }
                }

                for path in contents.into_iter().filter(|p| p.exists()) {
                    if path.is_dir() {
                        let _ = package_builder
                            .add_dir_contents(&self.env_vars, &path)
                            .map_err(|e| trace!("add_dir_contents error {path:?}: {e:?}"));
                    }
                }
            }

            _ => {
                package_builder.add_executable_and_deps(&self.env_vars, &self.executable)?;
            }
        }

        // Return the builder so the archive can be lazily created, depending
        // on whether the scheduler reports it already has the toolchain or not
        Ok(Arc::new(package_builder))
    }
}

/// The cache is versioned by the inputs to `HashKeyParams`.
pub const CACHE_VERSION: &[u8] = b"12";

/// Environment variables that are factored into the cache key.
static CACHED_ENV_VARS: LazyLock<HashSet<&'static OsStr>> = LazyLock::new(|| {
    [
        // SCCACHE_C_CUSTOM_CACHE_BUSTER has no particular meaning behind it,
        // serving as a way for the user to factor custom data into the hash.
        // One can set it to different values for different invocations
        // to prevent cache reuse between them.
        "SCCACHE_C_CUSTOM_CACHE_BUSTER",
        "MACOSX_DEPLOYMENT_TARGET",
        "IPHONEOS_DEPLOYMENT_TARGET",
        "TVOS_DEPLOYMENT_TARGET",
        "WATCHOS_DEPLOYMENT_TARGET",
        "SDKROOT",
        "CCC_OVERRIDE_OPTIONS",
    ]
    .iter()
    .map(OsStr::new)
    .collect()
});

/// Parameters for computing a hash key for C/C++ compilation caching.
///
/// Construct with required fields via [`HashKeyParams::new`], then add optional
/// fields with builder methods.
///
/// # Example
/// ```ignore
/// let hash = HashKeyParams::new(
///     "abc123",          // compiler_digest
///     Language::C,        // language
///     &args,             // arguments
///     preprocessor_output // ProcessOutputStream
/// )
/// .with_extra_hashes(&extra)
/// .with_plusplus(true)
/// .compute();
/// ```
// #[derive(Debug, Clone)]
pub struct HashKeyParams<'a> {
    compiler_digest: &'a str,
    language: Language,
    arguments: &'a [OsString],
    preprocessor_output: Pin<Box<ProcessOutputStream>>,
    extra_hashes: &'a [String],
    env_vars: &'a [(OsString, OsString)],
    plusplus: bool,
    basedirs: &'a [Vec<u8>],
}

impl<'a> HashKeyParams<'a> {
    /// Creates a new hash key parameter set with required fields.
    ///
    /// Optional fields have default values and can be set via `with_*` methods.
    ///
    /// # Arguments
    /// * `compiler_digest` - Hash of the compiler executable
    /// * `language` - Source language being compiled
    /// * `arguments` - Compiler arguments
    /// * `preprocessor_output` - Preprocessed source to hash
    pub fn new(
        compiler_digest: &'a str,
        language: Language,
        arguments: &'a [OsString],
        preprocessor_output: Pin<Box<ProcessOutputStream>>,
    ) -> Self {
        Self {
            compiler_digest,
            language,
            arguments,
            preprocessor_output,
            extra_hashes: &[],
            env_vars: &[],
            plusplus: false,
            basedirs: &[],
        }
    }

    /// Sets additional hash data to include in the cache key.
    pub fn with_extra_hashes(mut self, extra_hashes: &'a [String]) -> Self {
        self.extra_hashes = extra_hashes;
        self
    }

    /// Sets the environment variables to consider for caching.
    pub fn with_env_vars(mut self, env_vars: &'a [(OsString, OsString)]) -> Self {
        self.env_vars = env_vars;
        self
    }

    /// Sets whether this is a C++ compiler (affects clang/clang++ distinction).
    pub fn with_plusplus(mut self, plusplus: bool) -> Self {
        self.plusplus = plusplus;
        self
    }

    /// Sets the base directories for path normalization.
    pub fn with_basedirs(mut self, basedirs: &'a [Vec<u8>]) -> Self {
        self.basedirs = basedirs;
        self
    }

    /// Computes the hash key based on the configured parameters.
    ///
    /// If `basedirs` are provided, paths in the preprocessor output will be normalized by
    /// stripping the longest matching basedir prefix. This enables cache hits across different
    /// absolute paths (similar to ccache's CCACHE_BASEDIR).
    ///
    /// # Note
    /// If you change any of the inputs to the hash, you should change `CACHE_VERSION`.
    pub async fn compute(self) -> Result<String> {
        let mut m = Digest::new();
        m.update(self.compiler_digest.as_bytes());
        // clang and clang++ have different behavior despite being byte-for-byte identical binaries, so
        // we have to incorporate that into the hash as well.
        m.update(&[self.plusplus as u8]);
        m.update(CACHE_VERSION);
        m.update(self.language.as_str().as_bytes());
        for arg in self.arguments {
            arg.hash(&mut HashToDigest { digest: &mut m });
        }
        for hash in self.extra_hashes {
            m.update(hash.as_bytes());
        }

        for (var, val) in self.env_vars.iter() {
            if CACHED_ENV_VARS.contains(var.as_os_str()) {
                var.hash(&mut HashToDigest { digest: &mut m });
                m.update(&b"="[..]);
                val.hash(&mut HashToDigest { digest: &mut m });
            }
        }

        // Hash the preprocessor output
        let (m, _) = self
            .preprocessor_output
            .try_fold(
                (m, self.basedirs.to_vec()),
                |(mut m, basedirs), lines| async move {
                    tokio::task::spawn_blocking(move || {
                        // Strip basedirs from preprocessor output if configured
                        m.update(&strip_basedirs(&lines, &basedirs)[..]);
                        Ok((m, basedirs))
                    })
                    .await?
                },
            )
            .await?;

        Ok(m.finish())
    }
}

#[cfg(test)]
mod test {
    use crate::{
        cache::StorageKind,
        config::{CacheConfigs, CacheModeConfig, Config, DiskCacheConfig},
        test::utils::*,
    };

    use super::*;

    use fs_err as fs;

    fn into_process_output_stream(buf: &[u8]) -> Pin<Box<ProcessOutputStream>> {
        Box::pin(futures::stream::iter([Ok(BytesMut::from(buf))]))
    }

    #[test]
    fn test_same_content() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED: &[u8] = b"hello world";
        let h1 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_plusplus_differs() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED: &[u8] = b"hello world";
        let h1 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .with_plusplus(true)
        .compute()
        .wait()
        .unwrap();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_header_differs() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED: &[u8] = b"hello world";
        let h1 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "abcd",
            Language::CHeader,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_plusplus_header_differs() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED: &[u8] = b"hello world";
        let h1 = HashKeyParams::new(
            "abcd",
            Language::Cxx,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .with_plusplus(true)
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "abcd",
            Language::CxxHeader,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .with_plusplus(true)
        .compute()
        .wait()
        .unwrap();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_executable_contents_differs() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED: &[u8] = b"hello world";
        let h1 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "wxyz",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_args_differs() {
        let abc = ovec!["a", "b", "c"];
        let xyz = ovec!["x", "y", "z"];
        let ab = ovec!["a", "b"];
        let a = ovec!["a"];

        const PREPROCESSED: &[u8] = b"hello world";
        let h_abc = HashKeyParams::new(
            "abcd",
            Language::C,
            &abc,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h_xyz = HashKeyParams::new(
            "abcd",
            Language::C,
            &xyz,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h_ab = HashKeyParams::new(
            "abcd",
            Language::C,
            &ab,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();
        let h_a = HashKeyParams::new(
            "abcd",
            Language::C,
            &a,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();

        assert_neq!(h_abc, h_xyz);
        assert_neq!(h_abc, h_ab);
        assert_neq!(h_abc, h_a);
    }

    #[test]
    fn test_hash_key_preprocessed_content_differs() {
        let args = ovec!["a", "b", "c"];
        let h1 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(&b"hello world"[..]),
        )
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(&b"goodbye"[..]),
        )
        .compute()
        .wait()
        .unwrap();
        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_env_var_differs() {
        let args = ovec!["a", "b", "c"];
        const PREPROCESSED: &[u8] = b"hello world";
        for var in CACHED_ENV_VARS.iter() {
            let h1 = HashKeyParams::new(
                "abcd",
                Language::C,
                &args,
                into_process_output_stream(PREPROCESSED),
            )
            .compute()
            .wait()
            .unwrap();

            let vars1 = vec![(OsString::from(var), OsString::from("something"))];
            let h2 = HashKeyParams::new(
                "abcd",
                Language::C,
                &args,
                into_process_output_stream(PREPROCESSED),
            )
            .with_env_vars(&vars1)
            .compute()
            .wait()
            .unwrap();

            let vars2 = vec![(OsString::from(var), OsString::from("something else"))];
            let h3 = HashKeyParams::new(
                "abcd",
                Language::C,
                &args,
                into_process_output_stream(PREPROCESSED),
            )
            .with_env_vars(&vars2)
            .compute()
            .wait()
            .unwrap();

            assert_neq!(h1, h2);
            assert_neq!(h2, h3);
        }
    }

    #[test]
    fn test_extra_hash_data() {
        let args = ovec!["a", "b", "c"];
        let extra_data = stringvec!["hello", "world"];
        const PREPROCESSED: &[u8] = b"hello world";

        let h1 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .with_extra_hashes(&extra_data)
        .compute()
        .wait()
        .unwrap();
        let h2 = HashKeyParams::new(
            "abcd",
            Language::C,
            &args,
            into_process_output_stream(PREPROCESSED),
        )
        .compute()
        .wait()
        .unwrap();

        assert_neq!(h1, h2);
    }

    #[test]
    fn test_hash_key_basedirs() {
        let args = ovec!["a", "b", "c"];
        let preprocessed1: &[u8] =
            b"# 1 \"/home/user1/project/src/main.c\"\nint main() { return 0; }";
        let preprocessed2: &[u8] =
            b"# 1 \"/home/user2/project/src/main.c\"\nint main() { return 0; }";
        let basedirs = [
            b"/home/user1/project".to_vec(),
            b"/home/user2/project".to_vec(),
        ];

        // Helper to compute with basedirs
        let hash_with_basedirs = |output: &[u8], dirs: &[Vec<u8>]| {
            HashKeyParams::new(
                "abcd",
                Language::C,
                &args,
                into_process_output_stream(output),
            )
            .with_basedirs(dirs)
            .compute()
            .wait()
            .unwrap()
        };

        // Test 1: Same hash with different absolute paths when basedir is used
        let h1 = hash_with_basedirs(preprocessed1, &basedirs);
        let h2 = hash_with_basedirs(preprocessed2, &basedirs);
        assert_eq!(h1, h2);

        // Test 2: Same hash with single basedir that matches each
        assert_eq!(
            hash_with_basedirs(preprocessed1, &basedirs[..1]),
            hash_with_basedirs(preprocessed2, &basedirs[1..])
        );

        // Test 3: Different hashes without basedir
        assert_neq!(
            HashKeyParams::new(
                "abcd",
                Language::C,
                &args,
                into_process_output_stream(preprocessed1)
            )
            .compute()
            .wait()
            .unwrap(),
            HashKeyParams::new(
                "abcd",
                Language::C,
                &args,
                into_process_output_stream(preprocessed2)
            )
            .compute()
            .wait()
            .unwrap()
        );

        // Test 4: Works for C++ files too
        let preprocessed_cpp1: &[u8] =
            b"# 1 \"/home/user1/project/src/main.cpp\"\nint main() { return 0; }";
        let preprocessed_cpp2: &[u8] =
            b"# 1 \"/home/user2/project/src/main.cpp\"\nint main() { return 0; }";
        let h_cpp1 = HashKeyParams::new(
            "abcd",
            Language::Cxx,
            &args,
            into_process_output_stream(preprocessed_cpp1),
        )
        .with_plusplus(true)
        .with_basedirs(&basedirs)
        .compute()
        .wait()
        .unwrap();
        let h_cpp2 = HashKeyParams::new(
            "abcd",
            Language::Cxx,
            &args,
            into_process_output_stream(preprocessed_cpp2),
        )
        .with_plusplus(true)
        .with_basedirs(&basedirs)
        .compute()
        .wait()
        .unwrap();
        assert_eq!(h_cpp1, h_cpp2);

        // Test 5: Doesn't work with trailing slash in basedir, they must be normalized in config
        let basedir_slash = b"/home/user1/project/".to_vec();
        let h_slash = hash_with_basedirs(preprocessed1, std::slice::from_ref(&basedir_slash));
        assert_neq!(h1, h_slash);

        // Test 6: Multiple basedirs - longest match wins
        let multi_basedirs = vec![
            b"/home/user1".to_vec(),
            b"/home/user1/project".to_vec(), // This should match (longest)
        ];
        assert_eq!(h1, hash_with_basedirs(preprocessed1, &multi_basedirs));
    }

    #[test]
    fn test_language_from_file_name() {
        fn t(extension: &str, expected: Language) {
            let path_str = format!("input.{extension}");
            let path = Path::new(&path_str);
            let actual = Language::from_file_name(path);
            assert_eq!(actual, Some(expected));
        }

        t("s", Language::Assembler);
        t("S", Language::AssemblerToPreprocess);
        t("sx", Language::AssemblerToPreprocess);

        t("c", Language::C);

        t("i", Language::CPreprocessed);

        t("C", Language::Cxx);
        t("cc", Language::Cxx);
        t("cp", Language::Cxx);
        t("cpp", Language::Cxx);
        t("CPP", Language::Cxx);
        t("cxx", Language::Cxx);
        t("c++", Language::Cxx);

        t("ii", Language::CxxPreprocessed);

        t("h", Language::GenericHeader);

        t("hh", Language::CxxHeader);
        t("H", Language::CxxHeader);
        t("hp", Language::CxxHeader);
        t("hxx", Language::CxxHeader);
        t("hpp", Language::CxxHeader);
        t("HPP", Language::CxxHeader);
        t("h++", Language::CxxHeader);
        t("tcc", Language::CxxHeader);

        t("m", Language::ObjectiveC);

        t("mi", Language::ObjectiveCPreprocessed);

        t("M", Language::ObjectiveCxx);
        t("mm", Language::ObjectiveCxx);

        t("mii", Language::ObjectiveCxxPreprocessed);

        t("cu", Language::Cuda);
        t("hip", Language::Hip);
    }

    #[test]
    fn test_language_from_file_name_none() {
        fn t(extension: &str) {
            let path_str = format!("input.{extension}");
            let path = Path::new(&path_str);
            let actual = Language::from_file_name(path);
            let expected = None;
            assert_eq!(actual, expected);
        }

        // gcc parses file-extensions as case-sensitive
        t("Cp");
        t("Cpp");
        t("Hp");
        t("Hpp");
        t("Mm");
        t("Cu");
    }

    #[test]
    fn test_read_write_local_preprocessor_cache() {
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
                let storage = StorageKind::Preprocessor
                    .create(&caches, &[])
                    .await
                    .unwrap();
                PreprocessorCacheEntry::default()
                    .put(storage.as_ref(), "test1")
                    .await
                    .unwrap();
            });
        }

        // Test Read-only
        {
            let caches = make_config(CacheModeConfig::ReadOnly).caches;
            runtime.block_on(async {
                let storage = StorageKind::Preprocessor
                    .create(&caches, &[])
                    .await
                    .unwrap();
                assert_eq!(
                    PreprocessorCacheEntry::default()
                        .put(storage.as_ref(), "test1",)
                        .await
                        .unwrap_err()
                        .to_string(),
                    "Cannot write to read-only storage"
                );
            });
        }
    }
}
