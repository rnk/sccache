# Bazel Remote Execution (REv2) support in sccache

Status: implemented behind the `dist-client-reapi` cargo feature.

    cargo build --features dist-client-reapi

Verified end to end against a Buildbarn deployment, compiling a real LLVM
translation unit with a relocatable clang shipped to the worker as an action
input.

## Goal

Let sccache act as a *bridge* onto a generic Bazel Remote Execution v2 service
(buildbarn, buildfarm, nativelink, BuildBuddy, EngFlow, RBE, ...) instead of
requiring the bespoke `sccache-dist` scheduler/server topology. A team gets
remote acceleration by pointing sccache at an REv2 endpoint; if they later
outgrow it, they graduate to a real Bazel/`recc`-style build without changing
remote infrastructure.

## What the daemon caches, and why that is the point

sccache runs as a long-lived local daemon, and almost everything expensive about
remote execution is a fact that does not change across the tens of thousands of
compiles in a build. The daemon is what makes those facts cheap, and that is the
single biggest reason this bridge is viable at all:

* **Per-file content digests, keyed by stat.** `PreprocessorDependenciesCache`
  (`src/compiler/preprocessor_cache.rs`) holds a daemon-lifetime LRU of
  `path -> (size, ctime, mtime, digest)`. If a header has not changed, its
  content hash is reused rather than the file being re-read and re-hashed. This
  is what stops a large C++ project from content-hashing the same thousand
  transitive headers once per translation unit. It also records whether a file
  contains `__DATE__`/`__TIME__`/`__TIMESTAMP__` and invalidates on the
  compilation timestamp for those, which a naive stat-keyed cache gets silently
  wrong.

* **Toolchain identity and packaging.** `ClientToolchains` maps a weak toolchain
  key to a packaged archive, so the compiler is collected, tarred and hashed
  once per daemon rather than once per compile.

* **The toolchain's input-root fragment.** `reapi::Client::toolchains` keeps the
  parsed `DirBuilder` for each toolchain, so a 128 MB toolchain archive is
  scanned once and every later action reuses the result.

* **Which blobs the server already has.** `Cas::known_present` remembers digests
  the server has acknowledged, so a build does not re-ask about the same
  toolchain and the same system headers for every action. It is dropped
  wholesale when the server reports a blob missing, so CAS eviction still
  recovers.

* **The compiler's implicit include search path.** Discovering it costs two
  compiler invocations; the result is memoized per compiler binary and flag set
  (`SYSTEM_INCLUDE_DIRS` in `src/compiler/gcc.rs`).

The residual per-compile cost of assembling the Merkle tree was measured at
**384 us** at `opt-level=3` for a realistic 754-node input root, against a
multi-second compile, and it runs in `spawn_blocking` so it cannot stall the
runtime. Making it incremental would require a dirty-flag memo on `DirBuilder`
whose failure mode is a silently stale action digest -- a bad trade for 384 us.

## Staging sources, not preprocessed blobs

sccache's dist packager already stages every file the preprocessor opened as a
separate tar entry, and this client turns each into its own CAS blob. The inputs
a conventional remote execution action needs were already on the wire -- what
was missing was for the action to *use* them.

By default (`stage = "sources"`) the remote action compiles the original source:

    clang++ -I... -DFOO -nostdinc -isystem... -c CommandLine.cpp -o CommandLine.cpp.o

The alternative (`stage = "preprocessed"`) is what sccache-dist does, and sends
one self-contained blob instead:

    clang++ -x c++-cpp-output <flags> -c CommandLine.dist.cpp -o CommandLine.cpp.o

Neither needs a dependency scanner -- the hard, slow part of `recc` -- because
the preprocessor reports exactly which files it opened, and that list is what
gets staged. That is strictly more accurate than a scanner.

Sources mode wins on uplink, which is the scarce direction on a developer's
connection. Measured against a Buildbarn deployment, compiling two LLVM
translation units that had never been built through it:

| | bytes uploaded |
| --- | --- |
| `stage = "preprocessed"` | 1,613,903 then 1,910,065 |
| `stage = "sources"` | 0 then 0 |

Zero, because the sources and system headers were already in the server's CAS
from other builds. A preprocessed blob is unique to one translation unit on one
machine and can never be shared with anyone; a source file is shared by every
client that ever compiles it. Statically, the preprocessed blob was 3.4 MB of a
6.9 MB payload for `CommandLine.cpp`, and sources mode means a one-header edit
re-uploads one header.

The cost is that the worker must reproduce the compiler's include search, which
is what the `-nostdinc -isystem...` above is doing; see "Include search" below.

`stage = "preprocessed"` is kept only as an escape hatch. Once sources mode has
proven itself, it and the `stage_sources` plumbing that threads it through the
compiler frontends should be deleted.

Measured on `llvm/lib/Support/CommandLine.cpp` from a large LLVM-based build:

| quantity | value |
| --- | --- |
| preprocessed `.ii` | 3.4 MB (398 KB zstd-19) |
| whole input root, preprocessed mode | 6.9 MB over 431 entries |
| whole input root, sources mode | 3.4 MB over 430 entries |
| object output | 535 KB |
| local compile time (host g++ 15.2) | 4.6 s |

3.4 MB is *just under* the REv2 4 MB `BatchUpdateBlobs` limit, so a streaming
`ByteStream.Write` path is mandatory, not optional.

## Include search

In sources mode the worker preprocesses, so it has to search for headers the
way this machine would. A compiler's *implicit* search directories are absolute
paths that only mean something locally, so left alone a worker resolves
`<type_traits>` against its own container image rather than the headers we
staged.

`--sysroot` is not sufficient: clang locates the libstdc++ headers by way of the
GCC installation directory, which a sysroot switch does not relocate. Instead
the search path is reproduced explicitly -- `-nostdinc` drops the built-in list,
and each discovered directory comes back as `-isystem`, in order, rewritten to
point inside the input root.

The directories are discovered by asking the compiler twice, once normally and
once with `-nostdinc`, and subtracting; the difference is exactly the implicit
list, which avoids having to know which of the user's own flags were include
paths. The result is memoized in the daemon per compiler binary and flag set.

### Known limitation: `#include_next` and unstaged directories

The staged set is the set of files the local preprocessor actually opened. A
directory that exists locally but contributed no header is not in the input
root, so an `#include_next` chain, or a lookup that depends on a directory
merely *existing*, can resolve differently on the worker.

This is the standard transparency risk of include-scanning remote execution and
is shared with Bazel and `recc`. It is accepted rather than solved. An include
path that is not staged is left as an absolute path, so it does not silently
bind to a same-named directory inside the input root.


## Where this plugs in

sccache already has exactly the right seam: `dist::Client` in `src/dist/mod.rs`.
It is constructed in exactly **one** place, `src/server.rs:367`. Everything
above it -- `DistCompile::into_result` in `src/compiler/compiler.rs:1329`, the
retry loop, `InputsPackager`, `ToolchainPackager`, local fallback -- is
client-agnostic. So this is a *second implementation of an existing trait*, not
a new code path through the compiler frontends.

`src/compiler/*.rs` needs **zero changes**.

### The tar trick

`InputsPackager::write_inputs` hands us a zlib'd **tar** stream. `pkg::tar_safe_path`
(`src/dist/pkg.rs:778`) already strips the leading `/` from every entry. A tar of
root-relative paths, with modes and symlinks, *is* an REv2 input root. So the
REv2 client parses the tar it is already given and converts entries directly to
`FileNode`/`SymlinkNode`/`DirectoryNode`. No frontend changes, no second
packaging path.

### Path model

REv2 requires `working_directory` and `output_paths` to be **relative to the
input root**, and an absolute path in `arguments` would escape the input root
entirely (the compiler would write to the worker's real `/work/...` and the
output would never be collected).

Mapping, on Linux where dist paths == local absolute paths:

| sccache | REv2 |
| --- | --- |
| `CompileCommand.cwd` = `/home/user/project/build` | `working_directory` = `""` (input root) |
| `arguments` absolute paths | leading `/` stripped |
| `executable` | relative path into input root, or absolute-into-image |
| `outputs: Vec<String>` (dist-absolute) | `output_paths`, leading `/` stripped |

Rewrite rule for `arguments`: strip the leading `/` only for args that are
*known* paths -- the compile input, anything in `outputs`, or a path present in
the input-root tree we just built. Do **not** blanket-strip every arg starting
with `/`. This is a small, unit-testable function.

Paths are lexically normalized (`.` and `..` collapsed) before that lookup.
Build systems routinely emit an interior `..`: LLVM's CMake produces
`-I<src>/llvm/../third-party/siphash/include`, while the tar entries we stage
use the canonical spelling. Skipping normalization fails *silently* -- the
lookup misses, the rewriter concludes the argument does not name anything we
shipped, leaves it absolute, and the worker resolves it against its own
filesystem, where the header does not exist. The normalization is purely
lexical, so it is wrong when an interior component is a symlink to somewhere
else; that is the same caveat every build system that canonicalizes `-I` flags
lives with.

`argv[0]` is relative (`work/toolchain/bin/clang`) and contains a `/`, so
`execve` resolves it against the cwd (= input root) rather than `PATH`.

## Toolchain model

Scope decision: **assume a well-behaved, relocatable compiler.** Verified
against a Chromium clang 22 build:

* single self-contained binary, `-cc1` runs in-process, integrated assembler
  (no `cc1plus`/`as` fork by absolute path, unlike GCC)
* max symbol version `GLIBC_2.17`; `NEEDED` is only
  `libc/libm/libdl/librt/libpthread/libz/libgcc_s`
* finds its own resources via `/proc/self/exe`, so it is position-independent
  on disk
* for `-x c++-cpp-output` it needs no resource-dir headers at all

Consequence: shipping the compiler as an input works **without** requiring the
worker to `chroot` into the input root. That is what makes the "generic RE
service" story real -- no bespoke worker image needed. The clang in the
Any clang built this way has the same property.

Two configurable modes:

* `toolchain = "inputs"` (default): the packaged toolchain is merged into the
  input root and `argv[0]` is relative. `CToolchainPackager`
  (`src/compiler/c.rs:1818`) already produces a root-relative tar containing the
  binary and its deps -- we reuse it verbatim, unpacking it to CAS blobs. Blobs
  are deduplicated by `FindMissingBlobs`, so the toolchain uploads once per
  session, not once per action. The toolchain `Directory` digest is cached
  locally keyed by `Toolchain.archive_id`.
* `toolchain = "image"`: `argv[0]` stays absolute and must exist in the worker
  image (this is what `recc`-style setups do today). `put_toolchain` becomes a
  no-op and toolchain packaging is skipped entirely.

If a *non*-relocatable compiler (GCC, with its absolute `PT_INTERP` and
absolute `cc1plus` exec) is ever needed in `inputs` mode, the worker must be
configured with buildbarn's `chrootIntoInputRoot`. Out of scope; document it as
a known limitation.

## Dependencies

`prost` 0.13.5 is **already in `Cargo.lock`** (pulled in by `opendal`), as are
`h2` 0.4.5, `hyper` 1.8.1, `tower` 0.5.2, `tokio-stream`, `base64`, `tracing`.

* `tonic` with `default-features = false, features = ["codegen", "prost",
  "transport"]` -- no server, no `axum`. Net new crates: roughly `tonic`,
  `tonic-prost`, `hyper-timeout`, `pin-project`.
* **Generated protobuf code is checked in** under `src/dist/reapi/proto/`.
  `prost-build`/`tonic-build` require a `protoc` binary at build time, which
  this host does not even have; a build-script dependency on `protoc` would be
  a portability regression for sccache's release builds. Regeneration lives in
  `scripts/gen-reapi-proto.sh`, run by hand when the protos are bumped.
* All of it sits behind a new cargo feature `dist-client-reapi`, off by default
  in the sense that disabling it costs nothing. Vendored `.proto` files under
  `proto/` for provenance.

Protos needed: `build.bazel.remote.execution.v2`, `google.bytestream`,
`google.longrunning`, `google.rpc.Status`, plus `prost-types` well-knowns.

## File layout

    proto/                              vendored .proto sources
    scripts/gen-reapi-proto.sh          regeneration (needs protoc; run by hand)
    src/dist/reapi/mod.rs               `dist::Client` impl + config plumbing
    src/dist/reapi/proto/mod.rs         checked-in generated code
    src/dist/reapi/merkle.rs            tar stream -> Directory tree + blobs
    src/dist/reapi/cas.rs               digests, FindMissingBlobs,
                                        BatchUpdate/ReadBlobs, ByteStream
    src/dist/reapi/exec.rs              Action/Command assembly, Execute +
                                        WaitExecution driving, output fetch

## `dist::Client` -> REv2 mapping

| trait method | REv2 |
| --- | --- |
| `get_status` | `Capabilities.GetCapabilities`; validates digest function and `max_batch_total_size_bytes`; synthesizes a `SchedulerStatus` |
| `hash_toolchain` | reuse `dist/cache.rs`; in `image` mode skip packaging |
| `new_job` | parse inputs tar -> Merkle tree -> `FindMissingBlobs` -> upload missing -> stash the input-root digest under a local job id |
| `put_job` | re-upload (the retry path) |
| `put_toolchain` | `inputs` mode: unpack toolchain tar to blobs, cache the `Directory` digest by `archive_id`. `image` mode: `Success`, no-op |
| `run_job` | build `Command` + `Action`, upload, `Execute` (server-streaming `Operation`), drive to completion, fetch outputs -> `RunJobResponse::Complete` |
| `del_job` | drop local state |
| `fallback_to_local_compile` / `max_retries` / `request_timeout` / `rewrite_includes_only` | from config |

`ExecuteResponse.ActionResult` maps back cleanly: `exit_code` +
`stdout_raw`/`stdout_digest` + `stderr_*` -> `ProcessOutput`; `output_files` ->
`Vec<(String, OutputData)>` with the leading `/` restored on each path. The
existing code in `compiler.rs` then writes them to disk unchanged.

REv2 error mapping onto the existing retry taxonomy:
`UNAVAILABLE`/`ABORTED`/`RESOURCE_EXHAUSTED` -> `RetryableError`;
`FAILED_PRECONDITION` with a `PreconditionFailure` violation of type
`MISSING` -> `MissingJobInputs` (blob evicted from CAS -> re-upload and retry,
which is exactly what `compiler.rs:1487` already does); `INVALID_ARGUMENT` and
a nonzero compiler `exit_code` -> `Complete` (a real compile failure is a
successful action).

### Digest stability

The `Action` digest must be byte-stable or the remote action cache is useless.
Non-negotiable details: `Directory.files`/`directories`/`symlinks` each sorted
lexicographically **by name bytes**; `Command.environment_variables` sorted by
name; `output_paths` sorted; `Platform.properties` sorted by (name, value).
This gets a dedicated unit test with golden digests.

## Configuration

```toml
[dist.reapi]
url = "grpc://remote.example.com:50051"
instance_name = "main"
toolchain = "inputs"              # or "image"
digest_function = "sha256"
compress_blobs = true             # zstd compressed-blobs/ resource names
action_timeout_secs = 600
skip_cache_lookup = false
[dist.reapi.platform]
OSFamily = "Linux"
```

Env overrides follow the existing convention: `SCCACHE_DIST_REAPI_URL`,
`SCCACHE_DIST_REAPI_INSTANCE`, `SCCACHE_DIST_REAPI_TOOLCHAIN`, ...

Selection in `src/server.rs`: if `dist.reapi.url` is set, construct
`reapi::Client`; else fall back to today's `dist::http::Client`. Everything
else about `[dist]` (`fallback_to_local_compile`, `max_retries`,
`rewrite_includes_only`, `toolchain_cache_size`) is shared.

`grpcs://` selects TLS via the `rustls` already in the tree. mTLS is a later
addition.

Auth: a token from the existing `dist.auth` config is sent as
`authorization: Bearer <token>`. That covers the common case, but real
deployments sit behind proxies that want something else, so `[dist.reapi.headers]`
sets arbitrary gRPC metadata on every request and overrides the token-derived
header:

```toml
[dist.reapi.headers]
authorization = "Basic dXNlcjpodW50ZXIy"
```

This is deliberately general rather than a menu of auth schemes. It also
covers deployments that route on their own headers. Bazel's `.netrc` support
produces exactly the `Basic` form above, so pointing sccache at a cluster
Bazel already talks to is a matter of base64-encoding the same credential; the
`--remote_header=` flags in a `.bazelrc` map one-to-one onto this table.

## What landed

    proto/                              vendored .proto sources
    scripts/gen-reapi-proto/            standalone regenerator (protox, no protoc)
    src/dist/reapi/mod.rs               the `dist::Client` impl
    src/dist/reapi/paths.rs             absolute dist paths <-> input-root paths
    src/dist/reapi/merkle.rs            tar stream -> Directory tree + blobs
    src/dist/reapi/cas.rs               digests, FindMissingBlobs, Batch*, ByteStream
    src/dist/reapi/exec.rs              Action/Command assembly, Execute driving
    src/dist/reapi/tests.rs             a working fake REv2 service
    src/dist/reapi/proto/               checked-in generated bindings

`src/compiler/*.rs` is untouched, as intended. The only edits outside
`src/dist/reapi/` are the config surface (`src/config.rs`), client selection
(`src/server.rs`), module registration plus one relaxed `cfg` on
`OutputData::try_from_reader` (`src/dist/mod.rs`), and the manifest.

### Testing

`src/dist/reapi/tests.rs` stands up a fake REv2 service -- CAS, ByteStream,
Capabilities and Execution -- that materializes the input root into a temporary
directory, runs the command, and collects the declared outputs. The whole
client path is therefore testable in CI with no network, no container runtime
and no Buildbarn. It covers: an end-to-end compile with a relative `argv[0]`,
action digest stability across identical work, blobs uploading exactly once,
the `ByteStream` path for blobs over the batch limit, a failing compiler coming
back as a *completed* action, an unknown job reporting missing inputs, `image`
mode sending no toolchain, and a cache-only endpoint being rejected at startup.

`cargo test --features dist-client-reapi --lib dist::reapi` runs all of it.

### Two bugs the tests and the live run caught

Worth recording, because both are the kind that fail silently:

1. **`File::try_clone` shares the file cursor.** `ingest_archive` reads the
   inputs archive twice -- once to digest, once to upload what the server is
   missing. The second reader started at EOF, so the client uploaded *nothing*
   and every action failed with a missing blob. `rewound()` now seeks each
   fresh handle to zero.

2. **The packagers emit absolute symlink targets.** sccache's toolchain
   packager produces `work/toolchain/bin/clang++ -> /work/toolchain/bin/clang`,
   which is correct for sccache-dist, because it unpacks a job into an overlay
   rooted at `/`. An REv2 worker does not chroot, so the target escaped the
   input root, dangled, and `fork/exec` failed with a bare `no such file or
   directory`. `DirBuilder::insert_symlink` now re-anchors absolute targets
   relative to the symlink's own directory. REv2 servers also advertise a
   `symlink_absolute_path_strategy` and commonly reject absolute targets
   outright, so this is the spec-aligned behaviour too.

### Verification result

Compiling `llvm/lib/Support/CommandLine.cpp` against a Buildbarn deployment, in
both staging modes, with a relocatable clang shipped as an action input: the
resulting object has defined symbols identical to a local build.

The object is not byte-identical to a fused local compile, and should not be
expected to be: compiling preprocessed source *locally* differs from a fused
local compile in exactly the same way (349,079 bytes out of 432,664). That is
inherent to how sccache distributes work, not to remote execution. Defined
symbols match exactly.

The first run uploads the toolchain -- about 128 MB for Chromium's clang, since
`CToolchainPackager` collects the binary, its shared libraries, `as`, `objcopy`
and the loader. `FindMissingBlobs` means subsequent actions upload nothing but
the preprocessed source.

## Still to do

* zstd `compressed-blobs` resource names. `CommandLine.cpp` preprocesses to
  3.6 MB and compresses to 398 KB with zstd-19, so this is the single biggest
  remaining win for wide-area links.
* A concurrency limit on uploads. Today each job uploads serially within
  itself; parallel compiles are bounded only by sccache's own job limit.
* `--dist-status` reporting something more informative than the endpoint URL.
* Wire up a full build as the integration target.

## Explicit non-goals

* No dependency scanning / unpreprocessed remote compiles.
* No remote linking, no remote `ar`.
* No `ActionCache` write-through beyond what the server does on its own;
  sccache's own object cache stays the first-line cache.
* No `recc` replacement. This is the on-ramp, not the destination.
* nvcc sub-actions are left on the preprocessed path. `cicc`, `cudafe` and
  `ptxas` use languages for which `Language::needs_c_preprocessing()` is already
  false, so they already name their original input and preprocessing is a
  structural part of that compilation model. `stage_sources` is deliberately a
  no-op for them.
* MSVC is left on the preprocessed path.
* Only SHA-256. The client refuses to start against a server that does not
  advertise it, rather than silently producing digests the server cannot
  interpret.
* GCC in `toolchain = "inputs"` mode. GCC's `PT_INTERP` is absolute and it
  execs `cc1plus` by absolute path, so it is not relocatable into an input
  root. Use `toolchain = "image"`, or a worker configured with Buildbarn's
  `chrootIntoInputRoot`.
