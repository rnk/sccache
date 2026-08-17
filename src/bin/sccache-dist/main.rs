use std::env;
use std::sync::Arc;
use std::time::Duration;

#[cfg_attr(target_os = "freebsd", path = "build_freebsd.rs")]
mod build;

mod cmdline;
use cmdline::Command;

use sccache::{
    cache::cache::StorageKind,
    config::{
        scheduler as scheduler_config,
        server::{self as server_config, BuilderType},
    },
    dist::{
        self, BuilderIncoming, ServerToolchains, env_info,
        metrics::Metrics,
        scheduler::{self, SchedulerMetrics},
        server, tasks,
        token_check::new_client_auth_check,
    },
    errors::*,
};

// Only supported on x86_64/aarch64 Linux machines and on FreeBSD
#[cfg(not(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "freebsd"
)))]
fn main() {
    compile_error!("Distributed compilation is only supported on Linux/x86_64 and FreeBSD!");
}

// Only supported on x86_64/aarch64 Linux machines and on FreeBSD
#[cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "freebsd"
))]
fn main() {
    dist::init_logging();
    rustls::crypto::ring::default_provider()
        .install_default()
        .unwrap();

    let incr_env_strs = ["CARGO_BUILD_INCREMENTAL", "CARGO_INCREMENTAL"];
    incr_env_strs
        .iter()
        .for_each(|incr_str| match env::var(incr_str) {
            Ok(incr_val) if incr_val == "1" => {
                println!(
                    "sccache: incremental compilation is prohibited: Unset {incr_str} to continue."
                );
                std::process::exit(1);
            }
            _ => (),
        });

    let command = match cmdline::try_parse_from(env::args()) {
        Ok(cmd) => cmd,
        Err(e) => match e.downcast::<clap::error::Error>() {
            Ok(clap_err) => clap_err.exit(),
            Err(some_other_err) => {
                println!("sccache-dist: {some_other_err}");
                for source_err in some_other_err.chain().skip(1) {
                    println!("sccache-dist: caused by: {source_err}");
                }
                std::process::exit(1);
            }
        },
    };

    std::process::exit(match run(command) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("sccache-dist: error: {e}");

            for e in e.chain().skip(1) {
                eprintln!("sccache-dist: caused by: {e}");
            }
            2
        }
    });
}

fn run(command: Command) -> Result<()> {
    let num_cpus = sccache::util::num_cpus();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match command {
                Command::Scheduler(scheduler_config::Config {
                    client_auth,
                    heartbeat_interval_ms,
                    job_time_limit,
                    jobs,
                    keepalive,
                    max_body_size,
                    max_concurrent_streams,
                    message_broker,
                    metrics,
                    public_addr,
                    scheduler_id,
                    shutdown_timeout,
                    toolchains,
                }) => {
                    let metrics = Metrics::new(
                        metrics,
                        [
                            ("env".into(), env_info()),
                            ("type".into(), "scheduler".into()),
                            ("scheduler_id".into(), scheduler_id.clone()),
                        ]
                        .into(),
                    )?;

                    let jobs = StorageKind::Compilations
                        .create(&jobs, &[])
                        .await
                        .context("Failed to initialize jobs storage")?;

                    // Verify read/write access to jobs storage
                    match jobs.check().await {
                        Ok(sccache::cache::CacheMode::ReadWrite) => {}
                        _ => {
                            bail!("Scheduler jobs storage must be read/write")
                        }
                    }

                    let toolchains = StorageKind::Compilations
                        .create(&toolchains, &[])
                        .await
                        .context("Failed to initialize toolchain storage")?;

                    // Verify read/write access to toolchain storage
                    match toolchains.check().await {
                        Ok(sccache::cache::CacheMode::ReadWrite) => {}
                        _ => {
                            bail!("Scheduler toolchain storage must be read/write")
                        }
                    }

                    // Create ClientAuthCheck and bail on Err before registering with Celery
                    let client_auth_check = futures::future::try_join_all(
                        client_auth.into_iter().map(new_client_auth_check),
                    )
                    .await?;

                    let tasks = tasks::Tasks::scheduler(
                        &scheduler_id,
                        100 * num_cpus as u16,
                        job_time_limit,
                        message_broker,
                    )
                    .await?;

                    let scheduler = scheduler::Scheduler::new(
                        jobs,
                        SchedulerMetrics::new(metrics.clone()),
                        &scheduler_id,
                        tasks,
                        toolchains,
                    )?;

                    let (handle, server) =
                        dist::http::Scheduler::new(scheduler.clone(), client_auth_check).serve(
                            metrics,
                            public_addr,
                            keepalive,
                            max_body_size,
                            max_concurrent_streams,
                        );

                    scheduler
                        .start(
                            handle,
                            server,
                            Duration::from_millis(heartbeat_interval_ms),
                            Duration::from_secs(shutdown_timeout),
                        )
                        .await
                }

                Command::Server(server_config::Config {
                    message_broker,
                    builder,
                    cache_dir,
                    health_check_bind_addr,
                    heartbeat_interval_ms,
                    jobs,
                    max_per_core_load,
                    max_per_core_prefetch,
                    metrics,
                    server_id,
                    shutdown_timeout,
                    toolchain_cache_size,
                    toolchains,
                }) => {
                    let metrics = Metrics::new(
                        metrics,
                        [
                            ("env".into(), env_info()),
                            ("type".into(), "server".into()),
                            ("server_id".into(), server_id.clone()),
                        ]
                        .into(),
                    )?;

                    let jobs = StorageKind::Compilations
                        .create(&jobs, &[])
                        .await
                        .context("Failed to initialize jobs storage")?;

                    // Verify read/write access to jobs storage
                    match jobs.check().await {
                        Ok(sccache::cache::CacheMode::ReadWrite) => {}
                        _ => {
                            bail!("Server jobs storage must be read/write")
                        }
                    }

                    let toolchains = StorageKind::Compilations
                        .create(&toolchains, &[])
                        .await
                        .context("Failed to initialize toolchain storage")?;

                    // Verify toolchain storage
                    toolchains
                        .check()
                        .await
                        .context("Failed to initialize toolchain storage")?;

                    let occupancy = (num_cpus as f64 * max_per_core_load.max(0.0))
                        .floor()
                        .max(1.0) as usize;

                    let pre_fetch = (num_cpus as f64 * max_per_core_prefetch.max(0.0))
                        .floor()
                        .max(0.0) as usize;

                    let job_queue = Arc::new(tokio::sync::Semaphore::new(occupancy));

                    let should_inflate_toolchains = !matches!(builder, BuilderType::Docker { .. });
                    let builder = init_builder(builder, job_queue.clone()).await?;

                    let toolchains = Arc::new(ServerToolchains::new(
                        cache_dir.join("tc"),
                        toolchain_cache_size,
                        toolchains,
                        metrics.clone(),
                        should_inflate_toolchains,
                    ));

                    let tasks = tasks::Tasks::server(
                        &server_id,
                        (occupancy as u16).saturating_add(pre_fetch as u16),
                        message_broker,
                    )
                    .await?;

                    let server = server::Server::new(
                        builder,
                        jobs,
                        server::ServerState {
                            id: server_id.clone(),
                            queue: tasks.app().default_queue.clone(),
                            job_queue,
                            metrics: server::ServerMetrics::new(metrics.clone()),
                            num_cpus,
                            occupancy,
                            pre_fetch,
                            ..Default::default()
                        },
                        tasks,
                        toolchains,
                    )?;

                    // Report status every `heartbeat_interval_ms` milliseconds
                    server
                        .start(
                            Duration::from_millis(heartbeat_interval_ms),
                            Duration::from_secs(shutdown_timeout),
                            health_check_bind_addr,
                        )
                        .await
                }
            }
        })
}

async fn init_builder(
    config: BuilderType,
    job_queue: Arc<tokio::sync::Semaphore>,
) -> Result<Arc<dyn BuilderIncoming>> {
    match config {
        #[cfg(not(target_os = "freebsd"))]
        BuilderType::Docker {
            image,
            run_cmd,
            exec_cmd,
        } => Ok(Arc::new(
            build::DockerBuilder::new(image, run_cmd, exec_cmd, job_queue.clone())
                .await
                .context("Docker builder failed to start")?,
        ) as Arc<dyn BuilderIncoming>),
        #[cfg(not(target_os = "freebsd"))]
        BuilderType::Overlay {
            bwrap_path,
            build_dir,
            exec_cmd,
            lower_dirs,
            overlay_env,
        } => Ok(Arc::new(
            build::OverlayBuilder::new(
                bwrap_path,
                build_dir,
                exec_cmd.unwrap_or_default(),
                lower_dirs.unwrap_or_default(),
                overlay_env.unwrap_or_default(),
                job_queue.clone(),
            )
            .await
            .context("Overlay builder failed to start")?,
        ) as Arc<dyn BuilderIncoming>),
        #[cfg(target_os = "freebsd")]
        BuilderType::Pot {
            pot_fs_root,
            clone_from,
            pot_cmd,
            pot_clone_args,
        } => Ok(Arc::new(
            build::PotBuilder::new(
                pot_fs_root,
                clone_from,
                pot_cmd,
                pot_clone_args,
                job_queue.clone(),
            )
            .await
            .context("Pot builder failed to start")?,
        ) as Arc<dyn BuilderIncoming>),
        _ => bail!(
            "Builder type `{}` not supported on this platform",
            format!("{config:?}")
                .split_whitespace()
                .next()
                .unwrap_or("")
        ),
    }
}
