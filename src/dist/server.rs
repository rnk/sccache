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
    cache::{Cache, Storage},
    dist::{
        self, BuildError, BuildResult, BuilderIncoming, CompileCommand, RunJobError,
        RunJobResponse, ServerService, StatusUpdate, Toolchain, ToolchainService, job_inputs_key,
        job_result_key,
        metrics::{CountRecorder, GaugeRecorder, Metrics, TimeRecorder},
    },
    errors::*,
    util::{self, AsyncMulticast, AsyncMulticastArgs, AsyncMulticastFunc},
};
use async_trait::async_trait;
use futures::future::FutureExt;
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const NUM_CPUS_HISTO: &str = "sccache::server::num_cpus";
const CPU_USAGE_RATIO: &str = "sccache::server::cpu_usage_ratio";
const MEM_AVAIL_BYTES: &str = "sccache::server::mem_avail_bytes";
const MEM_TOTAL_BYTES: &str = "sccache::server::mem_total_bytes";
const MEM_USED_BYTES: &str = "sccache::server::mem_used_bytes";
const TOOLCHAIN_ERROR_COUNT: &str = "sccache::server::toolchain_error_count";
const GET_JOB_INPUTS_ERROR_COUNT: &str = "sccache::server::get_job_inputs_error_count";
const JOB_BUILD_ERROR_COUNT: &str = "sccache::server::job_build_error_count";
const PUT_JOB_RESULT_ERROR_COUNT: &str = "sccache::server::put_job_result_error_count";
const JOB_ACCEPTED_COUNT: &str = "sccache::server::job_accepted_count";
const JOB_LOADED_COUNT: &str = "sccache::server::job_loaded_count";
const JOB_FINISHED_COUNT: &str = "sccache::server::job_finished_count";
const JOB_PENDING_COUNT: &str = "sccache::server::job_pending_count";
const JOB_LOADING_COUNT: &str = "sccache::server::job_loading_count";
const GET_JOB_INPUTS_TIME: &str = "sccache::server::get_job_inputs_time";
const PUT_JOB_RESULT_TIME: &str = "sccache::server::put_job_result_time";
const GET_TOOLCHAIN_TIME: &str = "sccache::server::get_toolchain_time";
const LOAD_JOB_TIME: &str = "sccache::server::load_job_time";
const RUN_BUILD_TIME: &str = "sccache::server::run_build_time";
const RUN_JOB_TIME: &str = "sccache::server::run_job_time";

#[derive(Clone)]
pub struct ServerMetrics {
    jobs_accepted: Arc<AtomicU64>,
    jobs_loaded: Arc<AtomicU64>,
    jobs_finished: Arc<AtomicU64>,
    jobs_pending: Arc<GaugeRecorder>,
    jobs_loading: Arc<GaugeRecorder>,
    metrics: Metrics,
    sysinfo: Arc<std::sync::Mutex<sysinfo::System>>,
}

impl ServerMetrics {
    pub fn new(metrics: Metrics) -> Self {
        metrics::describe_gauge!(
            JOB_LOADING_COUNT,
            metrics::Unit::Count,
            "The number of accepted jobs for which this server is loading inputs and toolchains."
        );
        metrics::describe_gauge!(
            JOB_PENDING_COUNT,
            metrics::Unit::Count,
            "The number of accepted jobs that are fully loaded and queued to run/are currently running."
        );
        metrics::describe_histogram!(
            NUM_CPUS_HISTO,
            metrics::Unit::Count,
            "The total number of CPUs"
        );
        metrics::describe_histogram!(
            CPU_USAGE_RATIO,
            metrics::Unit::Percent,
            "The current system CPU usage percent (0-100)"
        );
        metrics::describe_histogram!(
            MEM_AVAIL_BYTES,
            metrics::Unit::Bytes,
            "The amount of free system memory"
        );
        metrics::describe_histogram!(
            MEM_TOTAL_BYTES,
            metrics::Unit::Bytes,
            "The total amount of system memory"
        );
        metrics::describe_histogram!(
            MEM_USED_BYTES,
            metrics::Unit::Bytes,
            "The amount of used system memory"
        );
        metrics::describe_counter!(
            TOOLCHAIN_ERROR_COUNT,
            metrics::Unit::Count,
            "The number of errors raised while loading toolchains."
        );
        metrics::describe_counter!(
            GET_JOB_INPUTS_ERROR_COUNT,
            metrics::Unit::Count,
            "The number of errors raised while loading job inputs."
        );
        metrics::describe_counter!(
            JOB_BUILD_ERROR_COUNT,
            metrics::Unit::Count,
            "The number of errors raised while running job builds."
        );
        metrics::describe_counter!(
            PUT_JOB_RESULT_ERROR_COUNT,
            metrics::Unit::Count,
            "The number of errors raised storing job results."
        );
        metrics::describe_counter!(
            JOB_ACCEPTED_COUNT,
            metrics::Unit::Count,
            "The total number of jobs accepted by this server (but not yet loaded or run)."
        );
        metrics::describe_counter!(
            JOB_LOADED_COUNT,
            metrics::Unit::Count,
            "The total number of jobs loaded by this server (but not yet run)."
        );
        metrics::describe_counter!(
            JOB_FINISHED_COUNT,
            metrics::Unit::Count,
            "The total number of jobs accepted, loaded, and run by this server."
        );
        metrics::describe_histogram!(
            GET_JOB_INPUTS_TIME,
            metrics::Unit::Seconds,
            "The time to load each job's inputs"
        );
        metrics::describe_histogram!(
            GET_TOOLCHAIN_TIME,
            metrics::Unit::Seconds,
            "The time to load each job's toolchain"
        );
        metrics::describe_histogram!(
            LOAD_JOB_TIME,
            metrics::Unit::Seconds,
            "The time to load each job's inputs and toolchains"
        );
        metrics::describe_histogram!(
            RUN_BUILD_TIME,
            metrics::Unit::Seconds,
            "The time to run each job's build"
        );
        metrics::describe_histogram!(
            PUT_JOB_RESULT_TIME,
            metrics::Unit::Seconds,
            "The time to store each job's results"
        );
        metrics::describe_histogram!(
            RUN_JOB_TIME,
            metrics::Unit::Seconds,
            "The time to load and build each job"
        );

        let jobs_pending = Arc::new(metrics.gauge(JOB_PENDING_COUNT));
        let jobs_loading = Arc::new(metrics.gauge(JOB_LOADING_COUNT));
        Self {
            metrics,
            jobs_pending,
            jobs_loading,
            ..Default::default()
        }
    }

    pub fn system_metrics(&self) -> (f32, u64, u64) {
        let mut sys = self.sysinfo.lock().unwrap();
        sys.refresh_cpu_specifics(sysinfo::CpuRefreshKind::nothing().with_cpu_usage());
        sys.refresh_memory_specifics(sysinfo::MemoryRefreshKind::nothing().with_ram());
        let cpu_usage = sys.global_cpu_usage();
        let mem_avail = sys.available_memory();
        let mem_total = sys.total_memory();
        self.metrics.histo(CPU_USAGE_RATIO, cpu_usage);
        self.metrics.histo(MEM_AVAIL_BYTES, mem_avail as f64);
        self.metrics.histo(MEM_TOTAL_BYTES, mem_total as f64);
        self.metrics
            .histo(MEM_USED_BYTES, mem_total.saturating_sub(mem_avail) as f64);
        (cpu_usage, mem_avail, mem_total)
    }

    pub fn inc_toolchain_error_count(&self) -> CountRecorder {
        self.metrics.count(TOOLCHAIN_ERROR_COUNT)
    }

    pub fn inc_get_job_inputs_error_count(&self) -> CountRecorder {
        self.metrics.count(GET_JOB_INPUTS_ERROR_COUNT)
    }

    pub fn inc_job_build_error_count(&self) -> CountRecorder {
        self.metrics.count(JOB_BUILD_ERROR_COUNT)
    }

    pub fn inc_put_job_result_error_count(&self) -> CountRecorder {
        self.metrics.count(PUT_JOB_RESULT_ERROR_COUNT)
    }

    pub fn inc_job_accepted_count(&self) -> CountRecorder {
        self.jobs_accepted
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.metrics.count(JOB_ACCEPTED_COUNT)
    }

    pub fn inc_job_loaded_count(&self) -> CountRecorder {
        self.jobs_loaded
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.metrics.count(JOB_LOADED_COUNT)
    }

    pub fn inc_job_finished_count(&self) -> CountRecorder {
        self.jobs_finished
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.metrics.count(JOB_FINISHED_COUNT)
    }

    pub fn get_job_inputs_timer(&self) -> TimeRecorder {
        self.metrics.timer(GET_JOB_INPUTS_TIME)
    }

    pub fn get_toolchain_timer(&self) -> TimeRecorder {
        self.metrics.timer(GET_TOOLCHAIN_TIME)
    }

    pub fn load_job_timer(&self) -> TimeRecorder {
        self.metrics.timer(LOAD_JOB_TIME)
    }

    pub fn run_build_timer(&self) -> TimeRecorder {
        self.metrics.timer(RUN_BUILD_TIME)
    }

    pub fn put_job_result_timer(&self) -> TimeRecorder {
        self.metrics.timer(PUT_JOB_RESULT_TIME)
    }

    pub fn run_job_timer(&self) -> TimeRecorder {
        self.metrics.timer(RUN_JOB_TIME)
    }
}

impl Default for ServerMetrics {
    fn default() -> Self {
        use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

        Self {
            jobs_accepted: Default::default(),
            jobs_loaded: Default::default(),
            jobs_finished: Default::default(),
            jobs_loading: Default::default(),
            jobs_pending: Default::default(),
            metrics: Metrics::default(),
            sysinfo: Arc::new(std::sync::Mutex::new(System::new_with_specifics(
                RefreshKind::nothing()
                    .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                    .with_memory(MemoryRefreshKind::nothing().with_ram()),
            ))),
        }
    }
}

pub struct JobInfo {
    created_at: Instant,
    reply_to: String,
}

#[derive(Clone)]
pub struct ServerState {
    pub alive: Arc<tokio::sync::watch::Sender<bool>>,
    pub id: String,
    pub queue: String,
    pub jobs: Arc<std::sync::Mutex<HashMap<String, JobInfo>>>,
    pub job_queue: Arc<tokio::sync::Semaphore>,
    pub metrics: ServerMetrics,
    pub num_cpus: usize,
    pub occupancy: usize,
    pub pre_fetch: usize,
}

impl ServerState {
    pub fn is_alive(&self) -> bool {
        *self.alive.borrow()
    }
}

impl Default for ServerState {
    fn default() -> Self {
        let (alive, _) = tokio::sync::watch::channel(true);
        Self {
            alive: Arc::new(alive),
            id: Default::default(),
            queue: Default::default(),
            job_queue: Arc::new(tokio::sync::Semaphore::new(1)),
            jobs: Default::default(),
            metrics: Default::default(),
            num_cpus: 1,
            occupancy: 1,
            pre_fetch: 0,
        }
    }
}

impl From<&ServerState> for StatusUpdate {
    fn from(state: &ServerState) -> Self {
        let (cpu_usage, mem_avail, mem_total) = state.metrics.system_metrics();

        state
            .metrics
            .metrics
            .histo(NUM_CPUS_HISTO, state.num_cpus as f64);

        let running = state
            .occupancy
            .saturating_sub(state.job_queue.available_permits()) as u64;

        let loading = state.metrics.jobs_loading.value();
        let pending = state.metrics.jobs_pending.value().saturating_sub(running);
        let accepted = state
            .metrics
            .jobs_accepted
            .load(std::sync::atomic::Ordering::SeqCst);
        let finished = state
            .metrics
            .jobs_finished
            .load(std::sync::atomic::Ordering::SeqCst);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_micros();

        let max_job_age = {
            let jobs = state.jobs.lock().unwrap();
            let now = Instant::now();
            jobs.values()
                .map(|j| now.duration_since(j.created_at).as_micros())
                .max()
                .unwrap_or(0)
        };

        StatusUpdate {
            id: state.id.clone(),
            queue: state.queue.clone(),
            info: dist::SysStats {
                cpu_usage,
                mem_avail,
                mem_total,
                num_cpus: state.num_cpus,
                occupancy: state.occupancy,
                pre_fetch: state.pre_fetch,
            },
            jobs: dist::JobStats {
                loading,
                pending,
                running,
                accepted,
                finished,
            },
            // Work around serde missing u128 type...
            // u128 works fine if I use rusty-celery's macros,
            // but not if I implement the Task trait directly.
            // The only difference I see is they use `celery::export::Deserialize`
            // instead of `serde::Deserialize`, but that does not solve the issue.
            timestamp: (
                // microseconds as seconds
                timestamp.saturating_div(1_000_000) as u64,
                // remainder microseconds as nanoseconds
                (timestamp % 1_000_000).saturating_mul(1_000) as u32,
            ),
            max_job_age: (
                // microseconds as seconds
                max_job_age.saturating_div(1_000_000) as u64,
                // remainder microseconds as nanoseconds
                (max_job_age % 1_000_000).saturating_mul(1_000) as u32,
            ),
        }
    }
}

impl AsyncMulticastArgs for Toolchain {
    type Key = String;
    fn hash(&self) -> Self::Key {
        self.archive_id.clone()
    }
}

struct LoadToolchainFn {
    toolchains: Arc<dyn ToolchainService>,
}

#[async_trait]
impl AsyncMulticastFunc<Toolchain, PathBuf> for LoadToolchainFn {
    async fn call(&self, tc: &Toolchain) -> Result<PathBuf> {
        self.toolchains.load_toolchain(tc).await
    }
}

#[derive(Clone)]
struct RunJobArgs {
    job_id: String,
    reply_to: String,
    toolchain: Toolchain,
    command: CompileCommand,
    outputs: Vec<String>,
}

impl AsyncMulticastArgs for RunJobArgs {
    type Key = String;
    fn hash(&self) -> Self::Key {
        self.job_id.clone()
    }
}

struct RunJobFunc {
    builder: Arc<dyn BuilderIncoming>,
    jobs_storage: Arc<dyn Storage>,
    state: ServerState,
    toolchains: AsyncMulticast<Toolchain, PathBuf>,
}

impl RunJobFunc {
    async fn get_toolchain_dir(&self, job_id: &str, toolchain: Toolchain) -> Result<PathBuf> {
        let _timer = self.state.metrics.get_toolchain_timer();
        // ServerToolchains retries internally, so no need to retry here
        self.toolchains
            .call(toolchain)
            .await
            .map(|(_, res)| res)
            .map_err(|err| {
                // Record toolchain errors
                if self.state.is_alive() {
                    self.state.metrics.inc_toolchain_error_count();
                    tracing::warn!("[run_job({job_id})]: Error loading toolchain: {err:?}");
                }
                err
            })
    }

    async fn get_job_inputs(&self, job_id: &str) -> Result<opendal::Buffer> {
        // Record get_job_inputs time
        let _timer = self.state.metrics.get_job_inputs_timer();
        self.jobs_storage
            .get(&job_inputs_key(job_id))
            .await
            .and_then(|res| match res {
                Cache::Hit(reader) => Ok(reader),
                _ => Err(anyhow!("Missing job inputs")),
            })
            .map_err(|err| {
                // Record get_job_inputs errors after retrying
                if self.state.is_alive() {
                    self.state.metrics.inc_get_job_inputs_error_count();
                    tracing::warn!("[run_job({job_id})]: Error retrieving job inputs: {err:?}");
                }
                err
            })
    }

    async fn run_build(
        &self,
        job_id: &str,
        toolchain_dir: PathBuf,
        inputs: opendal::Buffer,
        command: CompileCommand,
        outputs: Vec<String>,
    ) -> std::result::Result<BuildResult, BuildError> {
        // Record build time
        let _timer = self.state.metrics.run_build_timer();
        self.builder
            .run_build(job_id, &toolchain_dir, inputs, command, outputs)
            .await
            .map_err(|err| {
                // Record run_build errors
                if self.state.is_alive() {
                    self.state.metrics.inc_job_build_error_count();
                    tracing::warn!("[run_job({job_id})]: Build error: {err:?}");
                }
                err
            })
    }

    async fn load_job(
        &self,
        job_id: &str,
        toolchain: Toolchain,
    ) -> std::result::Result<(PathBuf, opendal::Buffer), RunJobError> {
        // Record load_job time
        let _timer = self.state.metrics.load_job_timer();
        let _loading = self.state.metrics.jobs_loading.increment();

        // Load and unpack the toolchain
        let toolchain_dir = self
            .get_toolchain_dir(job_id, toolchain)
            .await
            .map_err(|_| RunJobError::MissingToolchain)?;

        // Load job inputs into memory
        let inputs = self
            .get_job_inputs(job_id)
            .await
            .map_err(|_| RunJobError::MissingJobInputs)?;

        Ok((toolchain_dir, inputs))
    }

    async fn load_job_and_run_build(
        &self,
        job_id: &str,
        reply_to: &str,
        toolchain: Toolchain,
        command: CompileCommand,
        outputs: Vec<String>,
    ) -> std::result::Result<RunJobResponse, RunJobError> {
        // Record total run_job time
        let _timer = self.state.metrics.run_job_timer();

        // Add job
        self.state.jobs.lock().unwrap().insert(
            job_id.to_owned(),
            JobInfo {
                created_at: Instant::now(),
                reply_to: reply_to.to_owned(),
            },
        );

        // Increment the job_started counter
        self.state.metrics.inc_job_accepted_count();

        // Load the job toolchain and inputs
        let (toolchain_dir, inputs) = self.load_job(job_id, toolchain).await?;

        // Increment the job_loaded counter
        self.state.metrics.inc_job_loaded_count();

        // Increment the jobs_pending gauge
        let _pending = self.state.metrics.jobs_pending.increment();

        // Run the build
        self.run_build(job_id, toolchain_dir, inputs, command, outputs)
            .await
            .map(|result| {
                // If the build failed because the server was terminated,
                // report it as a server termination, not a failed build.
                if !self.state.is_alive() {
                    return RunJobResponse::server_terminated(&self.state.id);
                }
                if !result.output.success() && !result.output.exit() {
                    return RunJobResponse::build_process_killed(&self.state.id);
                }
                RunJobResponse::Complete {
                    result,
                    server_id: self.state.id.clone(),
                }
            })
            .map_err(|e| {
                if self.state.is_alive() {
                    if let BuildError::UnpackInputs(_) = e {
                        RunJobError::MissingJobInputs
                    } else {
                        RunJobError::Retryable(e.into())
                    }
                } else {
                    RunJobError::server_terminated()
                }
            })
    }
}

#[async_trait]
impl AsyncMulticastFunc<RunJobArgs, RunJobResponse> for RunJobFunc {
    async fn call(&self, args: &RunJobArgs) -> Result<RunJobResponse> {
        let RunJobArgs {
            job_id,
            reply_to,
            toolchain,
            command,
            outputs,
        } = args;
        if self.state.is_alive() {
            self.load_job_and_run_build(
                job_id,
                reply_to,
                toolchain.clone(),
                command.clone(),
                outputs.clone(),
            )
            .await
        } else {
            Err(RunJobError::server_terminated())
        }
        .map_err(|err| anyhow!(err))
    }
}

#[derive(Clone)]
pub struct Server {
    builder: Arc<dyn BuilderIncoming>,
    jobs_storage: Arc<dyn Storage>,
    state: ServerState,
    tasks: Arc<dyn dist::tasks::ServerTasks>,
    jobs: AsyncMulticast<RunJobArgs, RunJobResponse>,
    schedulers: Arc<futures::lock::Mutex<HashMap<String, Instant>>>,
}

impl Server {
    pub fn new(
        builder: Arc<dyn BuilderIncoming>,
        jobs_storage: Arc<dyn Storage>,
        state: ServerState,
        tasks: Arc<dyn dist::tasks::ServerTasks>,
        toolchains: Arc<dyn ToolchainService>,
    ) -> Result<Arc<Self>> {
        let this = Arc::new(Self {
            builder: builder.clone(),
            jobs_storage: jobs_storage.clone(),
            state: state.clone(),
            tasks: tasks.clone(),
            jobs: AsyncMulticast::new(RunJobFunc {
                builder,
                jobs_storage,
                state,
                toolchains: AsyncMulticast::new(LoadToolchainFn { toolchains }),
            }),
            schedulers: Default::default(),
        });

        this.tasks.set_service(this.clone())?;

        Ok(this)
    }

    pub async fn start(
        &self,
        heartbeat_interval: Duration,
        shutdown_timeout: Duration,
        health_check_bind_addr: Option<SocketAddr>,
    ) -> Result<()> {
        self.tasks.app().display_pretty().await;

        tracing::info!(
            "Server `{}` initialized to run {} parallel build job(s) and prefetch up to {} job(s) in the background",
            self.state.id,
            self.state.occupancy,
            self.state.pre_fetch,
        );

        crate::util::daemonize()?;

        // Start celery
        let celery = async {
            let res = self
                .tasks
                .app()
                .consume_from(&self.tasks.queues_to_consume())
                .await;
            tracing::info!("Celery shutdown");
            res.context("Celery error")
        };

        // Install our own handlers so pending jobs can bail early and record
        // server_terminated responses. This gives the client a chance to retry
        // the job or build locally.
        let sigint = async {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigint = signal(SignalKind::interrupt())?;
            let mut sigterm = signal(SignalKind::terminate())?;
            tokio::select! {
                biased;
                _ = sigint.recv() => {
                    tracing::info!("Received SIGINT");
                },
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM");
                },
            }
            Ok(())
        };

        // Install a health check listener for load balancers
        let health = if let Some(health_check_bind_addr) = health_check_bind_addr {
            self.health_check_listener(health_check_bind_addr).boxed()
        } else {
            futures::future::pending::<()>().boxed()
        };

        let status = util::spawn({
            let this = self.clone();
            async move {
                loop {
                    let _ = this.send_heartbeat().await;
                    tokio::time::sleep(heartbeat_interval).await;
                }
            }
        });

        // tokio::select! deterministically polls its futures in order, so
        // register celery first and sigint second so this sigint handler
        // doesn't override celery's internal sigint handler.
        //
        // This ensures celery progresses to "warm shutdown" mode, cancelling
        // the queue consumers, and doesn't attempt to pull more messages from
        // the queue while it is shutting down.
        let shutdown_celery = tokio::select! {
            biased;
            res = celery => res,
            res = sigint => res,
            // These should never resolve before either celery or sigint unless
            // a catastrophic error occurs (tokio dies somehow?). Just polling
            // it here so the tasks are cancelled once either of the above two
            // futures complete first.
            _ = health => Ok(()),
            _ = status => Ok(()),
        };

        // Drain the jobs map before broadcasting the shutdown signal.
        // This ensures the "Server terminated" response is written for all
        // jobs active when SIGINT was received.
        let jobs = self.state.jobs.lock().unwrap().drain().collect::<Vec<_>>();

        // Broadcast server is now shutting down.
        self.state.alive.send_replace(false);

        tracing::info!(
            "Waiting {}s for graceful shutdown",
            shutdown_timeout.as_secs()
        );

        let shutdown_jobs = tokio::time::timeout(shutdown_timeout, async {
            let jobs_label = if jobs.len() == 1 {
                format!("{} pending job", jobs.len())
            } else {
                format!("{} pending jobs", jobs.len())
            };

            tracing::info!("Cancelling {jobs_label}");

            // Kill build processes, remove containers, overlayfs dirs, etc.
            let builder = async {
                self.builder.shutdown().await;
                tracing::info!("Cancelled {jobs_label}");
                Ok(())
            };

            // Write "Server terminated" job responses and send the `job_finished` tasks
            // back to each interested scheduler.
            let replies = async {
                let replies =
                    jobs.into_iter()
                        .map(|(job_id, JobInfo { reply_to, .. })| async move {
                            tracing::info!(
                                "Sending server terminated response for job {job_id} to {reply_to}"
                            );
                            self.job_finished(
                                &job_id,
                                &reply_to,
                                &RunJobResponse::server_terminated(&self.state.id),
                            )
                            .await
                        });
                futures::future::try_join_all(replies)
                    .await
                    .inspect(|_res| {
                        tracing::info!("Reported server terminated for {jobs_label}");
                    })
            };

            tokio::try_join!(builder, replies)
        })
        .await
        .unwrap_or_else(|e| Err(e.into()));

        // Close broker connection.
        let shutdown_broker = self.tasks.app().close().await.context("Broker error");
        tracing::info!("Closed broker connection");

        shutdown_celery
            .and(shutdown_jobs)
            .and(shutdown_broker)
            .map(|_| tracing::info!("Server shutdown gracefully"))
    }

    async fn send_heartbeat(&self) -> Result<()> {
        // Prune schedulers we haven't seen in 90s
        let now = Instant::now();
        let timeout = Duration::from_secs(90);
        let schedulers = {
            let mut schedulers = self.schedulers.lock().await;
            schedulers.retain(|_, last_seen| now.duration_since(*last_seen) <= timeout);
            schedulers.keys().cloned().collect::<Vec<_>>()
        };
        let status: StatusUpdate = (&self.state).into();

        futures::future::try_join_all(
            std::iter::once(self.tasks.update_status(status.clone(), None)).chain(
                schedulers
                    .iter()
                    .map(|send_to| self.tasks.update_status(status.clone(), Some(send_to))),
            ),
        )
        .await
        .map_err(anyhow::Error::new)
        .map(|_| ())
    }

    async fn health_check_listener(&self, health_check_bind_addr: SocketAddr) -> () {
        use futures::stream::StreamExt;
        use hyper::{
            Response, StatusCode, server::conn::http1::Builder as HyperHttpBuilder,
            service::service_fn,
        };
        use hyper_util::rt::TokioIo;
        use tokio::sync::mpsc::{error::SendError, unbounded_channel};

        // Track all outstanding TCP response futures
        let (futures_tx, futures_rx) = unbounded_channel();

        let outer = util::spawn({
            let ok_service = service_fn(move |_| async move {
                Response::builder()
                    .status(StatusCode::OK)
                    .body(http_body_util::Full::<bytes::Bytes>::default())
            });

            async move {
                let listener = match tokio::net::TcpListener::bind(health_check_bind_addr).await {
                    Ok(listener) => listener,
                    Err(err) => {
                        tracing::warn!("Failed to create TCP listener: {err}");
                        return;
                    }
                };

                loop {
                    let stream = match listener.accept().await {
                        Ok((stream, _)) => TokioIo::new(stream),
                        Err(err) => {
                            tracing::warn!("Error accepting connection, ignoring request: {err}");
                            continue;
                        }
                    };

                    let inner = util::spawn(async move {
                        if let Err(err) = HyperHttpBuilder::new()
                            .serve_connection(stream, ok_service)
                            .await
                        {
                            tracing::warn!("Error serving healthcheck connection: {err}");
                        }
                    });

                    if let Err(SendError(handle)) = futures_tx.send(inner) {
                        // If we can't push the handle to the receiver,
                        // just detach and allow task to run untracked.
                        handle.detach();
                    }
                }
            }
        });

        let inners = tokio_stream::wrappers::UnboundedReceiverStream::new(futures_rx)
            .for_each_concurrent(None, |fut| async move {
                if let Err(err) = fut.await {
                    tracing::warn!("Healthcheck response future error: {err}");
                }
            });

        let _ = tokio::join!(outer, inners);
    }

    async fn put_job_result(&self, job_id: &str, result: &RunJobResponse) -> Result<()> {
        // Record put_job_result load time after retrying
        let _timer = self.state.metrics.put_job_result_timer();
        let result = bincode::serialize(result).map_err(|err| {
            tracing::warn!("[put_job_result({job_id})]: Error serializing result: {err:?}");
            err
        })?;
        self.jobs_storage
            .put(&job_result_key(job_id), result.into())
            .await
            .map(|_| ())
            .map_err(|err| {
                // Record put_job_result errors after retrying
                if self.state.is_alive() {
                    self.state.metrics.inc_put_job_result_error_count();
                    tracing::warn!("[run_job({job_id})]: Error storing job result: {err:?}");
                }
                err
            })
    }

    async fn job_finished(&self, job_id: &str, reply_to: &str, res: &RunJobResponse) -> Result<()> {
        self.state.metrics.inc_job_finished_count();

        // Store the job result for retrieval by a scheduler
        let _ = self.put_job_result(job_id, res).await;

        self.tasks
            .job_finished(job_id, reply_to, From::from(&self.state))
            .await
            .map_err(anyhow::Error::new)
            .map(|_| ())
    }
}

#[async_trait]
impl ServerService for Server {
    async fn run_job(
        &self,
        job_id: &str,
        reply_to: &str,
        toolchain: Toolchain,
        command: CompileCommand,
        outputs: Vec<String>,
    ) -> Result<RunJobResponse> {
        let reply_to = reply_to.to_owned();

        self.schedulers
            .lock()
            .await
            .entry(reply_to.clone())
            .and_modify(|last_seen| {
                *last_seen = Instant::now();
            })
            .or_insert_with(Instant::now);

        // Broadcast status after accepting the job
        self.tasks
            .update_status((&self.state).into(), Some(&reply_to))
            .await
            .map_err(anyhow::Error::new)
            .map(|_| ())?;

        self.jobs
            .call(RunJobArgs {
                job_id: job_id.to_owned(),
                reply_to,
                toolchain,
                command,
                outputs,
            })
            .await
            .map(|(_, res)| res)
    }

    async fn on_failure(&self, job_id: &str, reply_to: &str, job_err: RunJobError) -> Result<()> {
        // Clean up the build resources
        self.builder.finish_build(job_id).await;
        // Remove job and increment the job_finished counter
        if self.state.jobs.lock().unwrap().remove(job_id).is_some() {
            let server_id = self.state.id.clone();

            let job_res = match job_err {
                // Unrecoverable errors
                RunJobError::Fatal(e) => RunJobResponse::FatalError {
                    message: format!("{e:#}"),
                    server_id,
                },
                // Retryable errors
                RunJobError::Retryable(e) => RunJobResponse::RetryableError {
                    message: format!("{e:#}"),
                    server_id,
                },
                RunJobError::MissingJobInputs => RunJobResponse::MissingJobInputs { server_id },
                RunJobError::MissingJobResult => RunJobResponse::MissingJobResult { server_id },
                RunJobError::MissingToolchain => RunJobResponse::MissingToolchain { server_id },
            };

            tracing::warn!("[on_failure({job_id})]: {job_res:?}");

            // Store the job result and notify the interested scheduler
            self.job_finished(job_id, reply_to, &job_res).await
        } else {
            Ok(())
        }
    }

    async fn on_success(
        &self,
        job_id: &str,
        reply_to: &str,
        job_res: &RunJobResponse,
    ) -> Result<()> {
        // Clean up the build resources
        self.builder.finish_build(job_id).await;
        // Remove job and increment the job_finished counter
        if self.state.jobs.lock().unwrap().remove(job_id).is_some() {
            // Store the job result and notify the interested scheduler
            self.job_finished(job_id, reply_to, job_res).await
        } else {
            Ok(())
        }
    }

    async fn update_scheduler_status(&self, status: StatusUpdate) -> Result<()> {
        self.schedulers
            .lock()
            .await
            .entry(status.queue.clone())
            .and_modify(|last_seen| {
                *last_seen = Instant::now();
            })
            .or_insert_with(Instant::now);
        Ok(())
    }
}
