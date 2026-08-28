# Bazel Remote Execution v2

sccache can distribute compilations to any service that speaks the [Bazel
Remote Execution v2 API][rev2] (Buildbarn, BuildBuddy, Buildfarm, NativeLink,
and the rest), instead of to an `sccache-dist` scheduler. Everything else about
sccache is unchanged: the same local and remote result caches sit in front of
it, the same compiler frontends produce the command, and a failure still falls
back to compiling locally.

[rev2]: https://github.com/bazelbuild/remote-apis

This is behind the `dist-client-reapi` feature, which is **not** in the default
feature set:

    cargo build --release --features dist-client-reapi

## Configuration

```toml
[dist.reapi]
url = "grpcs://remote.example.com:443"
instance_name = "default"
toolchain = "inputs"

[dist.reapi.platform]
OSFamily = "Linux"

[dist.reapi.headers]
authorization = "Basic <base64 of user:password>"
```

Setting `dist.reapi.url` is what turns this on; it takes precedence over
`dist.scheduler_url`. Every field also has an `SCCACHE_DIST_REAPI_*`
environment variable, but prefer the config file: sccache's server inherits the
environment of whichever client happened to start it, so a server that gets
restarted mid-build can silently come back without the settings that configured
distribution. A config file is read by every server no matter who starts it.

### Authentication

`[dist.reapi.headers]` is arbitrary gRPC metadata, sent with every request.
These map one-to-one onto Bazel's `--remote_header=` flags, so the fastest way
to configure a deployment is to copy what its `.bazelrc` already does.

A token in `[dist.auth]` is sent as `Authorization: Bearer <token>`. Not every
deployment wants a bearer token — services fronted by a proxy that reads
`~/.netrc` typically want HTTP Basic instead — so anything in `headers`
overrides it.

Header *values* are never logged, including in the debug formatting of the
whole config.

### `toolchain`

- `inputs` (default) ships the compiler to the worker as part of the action's
  input root. This needs a **relocatable** compiler: one that finds its own
  resources relative to `argv[0]`/`/proc/self/exe`, and that does not exec
  helper binaries by absolute path. Clang satisfies this; GCC does not, because
  it execs `cc1plus` by an absolute path baked in at build time.
- `image` assumes the compiler already exists in the worker's container image
  at the same absolute path it has locally, and sends no toolchain at all. Use
  this for GCC, or with a worker configured to chroot into the input root.

## How it maps onto sccache

sccache dist-compiles *preprocessed* source: the remote action is
`cc -x c++-cpp-output ... -c in.ii -o out.o`, with no include paths and no
header lookups. That is why there is no dependency scanner here — there is
nothing to scan. It is also why the input root is small.

The one real impedance mismatch is paths. `sccache-dist` unpacks a job into an
overlay rooted at `/`, so absolute paths keep working. An REv2 worker instead
materializes an input root in a directory of its choosing and does *not* chroot
into it, so every path naming something we shipped has to be made relative or
it will escape the input root and resolve against the worker's own filesystem.
`src/dist/reapi/paths.rs` and `exec.rs` handle that rewriting.

Three consequences worth knowing about, all of which fail silently if you get
them wrong:

- **Symlink targets are re-anchored.** sccache's packagers emit absolute
  targets, which is correct under a chroot and dangling without one. REv2
  servers also advertise a `symlink_absolute_path_strategy` and commonly reject
  absolute targets outright.
- **The working directory must exist in the input tree**, per the spec.
  Nothing else puts it there, since the packagers stage the source and the
  directories leading to it, and an out-of-tree build compiles somewhere else
  entirely. Directories leading to the *outputs* are the worker's job and are
  deliberately not staged.
- **Interior `..` is collapsed before lookup.** Build systems routinely emit
  `-I<src>/foo/../bar`, and without normalization that argument does not match
  anything in the input root, stays absolute, and resolves on the worker.

## Staging inputs without an archive

`InputsPackager` can hand over a list of `InputEntry` values -- paths, not
bytes -- instead of writing a tar. The REv2 client uses that when the packager
supports it, which C and C++ do.

This matters because the two representations cost very different amounts. An
archive forces every input to be copied into a tar and then parsed back out of
it on every single job, which defeats any attempt to remember work across jobs.
Naming the files instead means they can be hashed where they lie, and a
`DigestCache` in the server memoizes those hashes keyed on
`(dev, ino, size, mtime, ctime)`. A header shared by a thousand translation
units is then hashed once and `stat`ed a thousand times, rather than read a
thousand times.

That stat tuple is a trust assumption, and the same one ccache's direct mode
and Bazel's local digest cache make: a file whose contents change without any
of those fields changing will be served from the cache. Including `ctime`
alongside `mtime` closes the cheap version of that hole, since restoring
`ctime` requires privileges.

Both paths are required to produce *byte-identical* input roots, and a test
asserts they yield the same REv2 action digest. If they ever diverged, a build
that switched between them would silently lose every remote action cache hit.

## Limitations

- **SHA-256 only.** The client refuses to start against a server that does not
  advertise it, rather than producing digests the server cannot interpret.
- **Uploads are serial within a job.** Parallel compiles are bounded only by
  sccache's own job limit.
- **A directory that exists locally but contributed no header is not staged**,
  so an `#include_next` chain, or a lookup that depends on a directory merely
  existing, can resolve differently on the worker. This is the standard
  transparency risk of include-based remote execution, shared with Bazel.
  Include paths that were not staged are left absolute so they cannot silently
  bind to a same-named directory inside the input root.
- **Objects are not byte-identical to a fused local compile.** Neither are
  locally-preprocessed ones; this is inherent to how sccache distributes work,
  not to remote execution. Defined symbols match.

## Regenerating the protobuf bindings

The `.proto` files under `proto/` are a verbatim copy of the upstream
specification, and the generated bindings are checked in, so no `protoc` is
needed to build sccache. After bumping the protos:

    cd scripts/gen-reapi-proto && cargo run -- ../..

The generator uses `protox`, a pure-Rust protobuf compiler.
