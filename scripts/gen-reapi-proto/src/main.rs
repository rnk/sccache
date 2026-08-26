use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).expect("usage: reapi-gen <repo-root>"));
    let proto_dir = root.join("proto");
    let out_dir = root.join("src/dist/reapi/proto");
    std::fs::create_dir_all(&out_dir)?;

    let files = [
        "build/bazel/remote/execution/v2/remote_execution.proto",
        "google/bytestream/bytestream.proto",
        "google/longrunning/operations.proto",
        "google/rpc/status.proto",
    ]
    .map(|f| proto_dir.join(f));

    let fds = protox::compile(files, [&proto_dir])?;

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .build_transport(false)
        .out_dir(&out_dir)
        .compile_fds_with_config(prost_build::Config::new(), fds)?;

    // `google.api` only carries the HTTP/gRPC transcoding annotations, which we
    // do not use. Everything else generated here is referenced by name from the
    // remote execution bindings and must be kept.
    let _ = std::fs::remove_file(out_dir.join("google.api.rs"));

    // The generated gRPC *server* stubs are only used by the in-crate fake
    // REv2 service in `src/dist/reapi/tests.rs`. Gating them behind `cfg(test)`
    // keeps tonic's `router`/server feature (and its axum dependency) out of
    // release builds.
    for e in std::fs::read_dir(&out_dir)? {
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let src = std::fs::read_to_string(&p)?;
        let gated = src.replace(
            "/// Generated server implementations.\npub mod ",
            "/// Generated server implementations.\n#[cfg(test)]\npub mod ",
        );
        std::fs::write(&p, gated)?;
    }
    for e in std::fs::read_dir(&out_dir)? {
        let p = e?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            let n = std::fs::metadata(&p)?.len();
            println!("generated {} ({n} bytes)", p.file_name().unwrap().to_string_lossy());
        }
    }
    Ok(())
}
