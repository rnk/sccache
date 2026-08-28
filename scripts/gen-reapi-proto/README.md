# gen-reapi-proto

Regenerates the checked-in prost/tonic bindings in `src/dist/reapi/proto/` from
the vendored `.proto` files in `proto/`.

This is a standalone crate, deliberately **excluded from the sccache
workspace**, so that neither `protoc` nor `prost-build`/`tonic-build` is needed
to build sccache itself. It uses [`protox`], a pure-Rust protobuf compiler, so
it does not need a `protoc` binary either.

Run it by hand after bumping the vendored protos:

    cd scripts/gen-reapi-proto && cargo run -- ../..

The generated gRPC *server* stubs are rewritten to be `#[cfg(test)]`-gated,
because they are only used by the in-crate fake REv2 service in
`src/dist/reapi/tests.rs`. That keeps tonic's `router` feature (and its `axum`
dependency) out of release builds.

[`protox`]: https://crates.io/crates/protox
