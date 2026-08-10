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

use async_trait::async_trait;

use futures::lock::Mutex;

use crate::{
    cache::{Cache, Storage},
    dist::{
        self, CompileCommand, JobStats, NewJobResponse, RunJobRequestV2, RunJobResponse,
        SchedulerService, SchedulerStatus, ServerStatus, StatusUpdate, SubmitToolchainResult,
        SysStats, Toolchain,
        http::bincode_deserialize,
        metrics::{CountRecorder, Metrics, TimeRecorder},
    },
    errors::*,
    util::{AsyncMulticast, AsyncMulticastArgs, AsyncMulticastFunc},
};

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::dist::{job_inputs_key, job_result_key};

const CPU_USAGE_RATIO: &str = "sccache::scheduler::cpu_usage_ratio";
const MEM_AVAIL_BYTES: &str = "sccache::scheduler::mem_avail_bytes";
const MEM_TOTAL_BYTES: &str = "sccache::scheduler::mem_total_bytes";
const MEM_USED_BYTES: &str = "sccache::scheduler::mem_used_bytes";
const HAS_JOB_INPUTS_TIME: &str = "sccache::scheduler::has_job_inputs_time";
const HAS_JOB_RESULT_TIME: &str = "sccache::scheduler::has_job_result_time";
const GET_JOB_RESULT_TIME: &str = "sccache::scheduler::get_job_result_time";
const DEL_JOB_INPUTS_TIME: &str = "sccache::scheduler::del_job_inputs_time";
const DEL_JOB_RESULT_TIME: &str = "sccache::scheduler::del_job_result_time";
const PUT_JOB_INPUTS_TIME: &str = "sccache::scheduler::put_job_inputs_time";
const PUT_TOOLCHAIN_TIME: &str = "sccache::scheduler::put_toolchain_time";
const DEL_TOOLCHAIN_TIME: &str = "sccache::scheduler::del_toolchain_time";
const PUT_JOB_INPUTS_ERROR_COUNT: &str = "sccache::scheduler::put_job_inputs_error_count";

#[derive(Clone)]
pub struct SchedulerMetrics {
    metrics: Metrics,
    sysinfo: Arc<std::sync::Mutex<sysinfo::System>>,
}

impl SchedulerMetrics {
    pub fn new(metrics: Metrics) -> Self {
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
        metrics::describe_histogram!(
            HAS_JOB_INPUTS_TIME,
            metrics::Unit::Seconds,
            "The time to check if each job's inputs exists."
        );
        metrics::describe_histogram!(
            HAS_JOB_RESULT_TIME,
            metrics::Unit::Seconds,
            "The time to check if each job's result exists."
        );
        metrics::describe_histogram!(
            GET_JOB_RESULT_TIME,
            metrics::Unit::Seconds,
            "The time to load each job's inputs."
        );
        metrics::describe_histogram!(
            DEL_JOB_INPUTS_TIME,
            metrics::Unit::Seconds,
            "The time to delete each job's inputs."
        );
        metrics::describe_histogram!(
            DEL_JOB_RESULT_TIME,
            metrics::Unit::Seconds,
            "The time to delete each job's result."
        );
        metrics::describe_histogram!(
            PUT_JOB_INPUTS_TIME,
            metrics::Unit::Seconds,
            "The time to store each job's inputs."
        );
        metrics::describe_histogram!(
            PUT_TOOLCHAIN_TIME,
            metrics::Unit::Seconds,
            "The time to store each job's toolchain."
        );
        metrics::describe_histogram!(
            DEL_TOOLCHAIN_TIME,
            metrics::Unit::Seconds,
            "The time to delete each job's toolchain."
        );
        metrics::describe_counter!(
            PUT_JOB_INPUTS_ERROR_COUNT,
            metrics::Unit::Count,
            "The number of errors raised storing job inputs."
        );
        Self {
            metrics,
            ..Default::default()
        }
    }

    pub fn inc_put_job_inputs_error_count(&self) -> CountRecorder {
        self.metrics.count(PUT_JOB_INPUTS_ERROR_COUNT)
    }

    pub fn has_job_inputs_timer(&self) -> TimeRecorder {
        self.metrics.timer(HAS_JOB_INPUTS_TIME)
    }

    pub fn has_job_result_timer(&self) -> TimeRecorder {
        self.metrics.timer(HAS_JOB_RESULT_TIME)
    }

    pub fn get_job_result_timer(&self) -> TimeRecorder {
        self.metrics.timer(GET_JOB_RESULT_TIME)
    }

    pub fn del_job_inputs_timer(&self) -> TimeRecorder {
        self.metrics.timer(DEL_JOB_INPUTS_TIME)
    }

    pub fn del_job_result_timer(&self) -> TimeRecorder {
        self.metrics.timer(DEL_JOB_RESULT_TIME)
    }

    pub fn put_job_inputs_timer(&self) -> TimeRecorder {
        self.metrics.timer(PUT_JOB_INPUTS_TIME)
    }

    pub fn put_toolchain_timer(&self) -> TimeRecorder {
        self.metrics.timer(PUT_TOOLCHAIN_TIME)
    }

    pub fn del_toolchain_timer(&self) -> TimeRecorder {
        self.metrics.timer(DEL_TOOLCHAIN_TIME)
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
}

impl Default for SchedulerMetrics {
    fn default() -> Self {
        use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

        Self {
            metrics: Metrics::default(),
            sysinfo: Arc::new(std::sync::Mutex::new(System::new_with_specifics(
                RefreshKind::nothing()
                    .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                    .with_memory(MemoryRefreshKind::nothing().with_ram()),
            ))),
        }
    }
}

#[derive(Clone, Debug)]
struct ServerInfo {
    // Last modified time (in the scheduler's reference frame)
    pub m_time: Instant,
    // Last modified time (in the server's reference frame)
    pub u_time: SystemTime,
    // Server performance stats
    pub info: SysStats,
    // Server job stats
    pub jobs: JobStats,
    // The age of the oldest active job (in seconds)
    pub max_job_age: u64,
    // The server-specific queue
    pub queue: String,
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

struct RunJobFn {
    jobs: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<RunJobResponse>>>>,
    tasks: Arc<dyn dist::tasks::SchedulerTasks>,
}

#[async_trait]
impl AsyncMulticastFunc<RunJobArgs, RunJobResponse> for RunJobFn {
    async fn call(&self, args: &RunJobArgs) -> Result<RunJobResponse> {
        let RunJobArgs {
            job_id,
            reply_to,
            toolchain,
            command,
            outputs,
        } = args;

        let (tx, rx) = tokio::sync::oneshot::channel::<RunJobResponse>();
        self.jobs.lock().await.insert(job_id.to_owned(), tx);

        let res = self
            .tasks
            .run_job(job_id, reply_to, toolchain, command, outputs)
            .await
            .map_err(anyhow::Error::new);

        if let Err(err) = res {
            self.jobs.lock().await.remove(job_id);
            Err(err)
        } else {
            rx.await.map_err(anyhow::Error::new)
        }
    }
}

#[derive(Clone)]
pub struct Scheduler {
    jobs_storage: Arc<dyn Storage>,
    jobs: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<RunJobResponse>>>>,
    metrics: SchedulerMetrics,
    scheduler_id: String,
    servers: Arc<Mutex<HashMap<String, ServerInfo>>>,
    tasks: Arc<dyn dist::tasks::SchedulerTasks>,
    toolchains: Arc<dyn Storage>,
    run_job: AsyncMulticast<RunJobArgs, RunJobResponse>,
}

impl Scheduler {
    pub fn new(
        jobs_storage: Arc<dyn Storage>,
        metrics: SchedulerMetrics,
        scheduler_id: &str,
        tasks: Arc<dyn dist::tasks::SchedulerTasks>,
        toolchains: Arc<dyn Storage>,
    ) -> Result<Arc<Self>> {
        let jobs = Arc::new(Mutex::new(HashMap::new()));
        let this = Arc::new(Self {
            jobs_storage,
            jobs: jobs.clone(),
            metrics,
            scheduler_id: scheduler_id.to_owned(),
            servers: Arc::new(Mutex::new(HashMap::new())),
            tasks: tasks.clone(),
            toolchains,
            run_job: AsyncMulticast::new(RunJobFn {
                jobs: jobs.clone(),
                tasks: tasks.clone(),
            }),
        });

        this.tasks.set_service(this.clone())?;

        Ok(this)
    }

    pub async fn start(
        &self,
        handle: axum_server::Handle<SocketAddr>,
        server: impl futures::Future<Output = Result<()>>,
        heartbeat_interval: Duration,
        shutdown_timeout: Duration,
    ) -> Result<()> {
        let celery = async {
            let res = self
                .tasks
                .app()
                .consume_from(&self.tasks.queues_to_consume())
                .await;
            tracing::info!("Celery shutdown");
            res.context("Celery error")
        };

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

        let status = tokio::spawn({
            let this = self.clone();
            async move {
                loop {
                    let _ = this.send_heartbeat().await;
                    tokio::time::sleep(heartbeat_interval).await;
                }
            }
        });

        // Wait for Celery or sigint, then shutdown Axum and Celery
        let shutdown_celery = async move {
            // tokio::select! deterministically polls its futures in order, so
            // register celery first and sigint second so this sigint handler
            // doesn't override celery's internal sigint handler.
            //
            // This ensures celery progresses to "warm shutdown" mode, cancelling
            // the queue consumers, and doesn't attempt to pull more messages
            // from the queue while it is shutting down.
            let res = tokio::select! {
                biased;
                res = celery => res,
                res = sigint => res,
                // This should never resolve before either celery or sigint unless
                // a catastrophic error occurs (tokio dies somehow?). Just polling
                // it here so the task is cancelled once either of the above two
                // futures complete first.
                _ = status => Ok(()),
            };

            tracing::info!(
                "Waiting {}s for graceful shutdown",
                shutdown_timeout.as_secs()
            );

            // Signal axum to shutdown
            handle.graceful_shutdown(Some(shutdown_timeout));

            res
        };

        // Wait for axum shutdown
        let shutdown_server = async move { server.await.context("Server error") };

        self.tasks.app().display_pretty().await;

        tracing::info!("Scheduler `{}` initialized", self.scheduler_id);

        crate::util::daemonize()?;

        // Wait for celery and/or server shutdown
        let shutdown_server = tokio::try_join!(shutdown_celery, shutdown_server);

        // Close the broker connection
        let shutdown_broker = self.tasks.app().close().await.context("Broker error");
        tracing::info!("Closed broker connection");

        shutdown_server
            .and(shutdown_broker)
            .map(|_| tracing::info!("Scheduler shutdown gracefully"))
    }

    async fn send_heartbeat(&self) -> Result<()> {
        let (cpu_usage, mem_avail, mem_total) = self.metrics.system_metrics();
        let status = self.get_status().await?;
        let status = StatusUpdate {
            id: self.scheduler_id.clone(),
            queue: self.tasks.app().default_queue.clone(),
            info: dist::SysStats {
                cpu_usage,
                mem_avail,
                mem_total,
                num_cpus: status.info.num_cpus,
                occupancy: status.info.occupancy,
                pre_fetch: status.info.pre_fetch,
            },
            jobs: status.jobs,
            max_job_age: {
                let max_job_age = status
                    .servers
                    .iter()
                    .map(|s| s.max_job_age)
                    .max()
                    .unwrap_or(0);
                (
                    // microseconds as seconds
                    max_job_age.saturating_div(1_000_000),
                    // remainder microseconds as nanoseconds
                    (max_job_age % 1_000_000).saturating_mul(1_000) as u32,
                )
            },
            timestamp: {
                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or(Duration::from_secs(0))
                    .as_micros();
                (
                    // microseconds as seconds
                    timestamp.saturating_div(1_000_000) as u64,
                    // remainder microseconds as nanoseconds
                    (timestamp % 1_000_000).saturating_mul(1_000) as u32,
                )
            },
        };

        let servers = self
            .servers
            .lock()
            .await
            .values()
            .map(|s| s.queue.clone())
            .collect::<Vec<_>>();

        futures::future::try_join_all(
            std::iter::once(self.tasks.update_status(status.clone(), None)).chain(
                servers
                    .iter()
                    .map(|send_to| self.tasks.update_status(status.clone(), Some(send_to))),
            ),
        )
        .await
        .map_err(anyhow::Error::new)
        .map(|_| ())
    }

    fn prune_servers(servers: &mut HashMap<String, ServerInfo>) {
        let now = Instant::now();
        // Prune servers we haven't seen in 90s
        let timeout = Duration::from_secs(90);
        servers.retain(|_, server| now.duration_since(server.m_time) <= timeout);
    }

    async fn has_job_inputs(&self, job_id: &str) -> bool {
        // Record has_job_inputs time
        let _timer = self.metrics.has_job_inputs_timer();
        self.jobs_storage.has(&job_inputs_key(job_id)).await
    }

    async fn has_job_result(&self, job_id: &str) -> bool {
        // Record has_job_result time
        let _timer = self.metrics.has_job_result_timer();
        self.jobs_storage.has(&job_result_key(job_id)).await
    }

    async fn get_job_result(&self, job_id: &str) -> Result<RunJobResponse> {
        // Record get_job_result time
        let _timer = self.metrics.get_job_result_timer();
        // Retrieve the result
        let result = self
            .jobs_storage
            .get(&job_result_key(job_id))
            .await
            .and_then(|res| match res {
                Cache::Hit(result) => Ok(result),
                _ => Err(anyhow!("Missing job result")),
            })
            .map_err(|err| {
                tracing::warn!("[get_job_result({job_id})]: Error loading stream: {err:?}");
                err
            })?;

        // Deserialize the result
        bincode_deserialize(result).await.map_err(|err| {
            tracing::warn!("[get_job_result({job_id})]: Error deserializing result: {err:?}");
            err
        })
    }

    async fn del_job_inputs(&self, job_id: &str) -> Result<()> {
        // Record del_job_inputs time
        let _timer = self.metrics.del_job_inputs_timer();
        // Delete the inputs
        self.jobs_storage
            .del(&job_inputs_key(job_id))
            .await
            .map_err(|e| {
                tracing::warn!("[del_job_inputs({job_id})]: Error deleting job inputs: {e:?}");
                e
            })
    }

    async fn del_job_result(&self, job_id: &str) -> Result<()> {
        // Record del_job_result time
        let _timer = self.metrics.del_job_result_timer();
        // Delete the result
        self.jobs_storage
            .del(&job_result_key(job_id))
            .await
            .map_err(|e| {
                tracing::warn!("[del_job_result({job_id})]: Error deleting job result: {e:?}");
                e
            })
    }

    async fn put_job_inputs(&self, job_id: &str, inputs: opendal::Buffer) -> Result<()> {
        // Record put_job_inputs time
        let _timer = self.metrics.put_job_inputs_timer();
        // Store the job inputs
        self.jobs_storage
            .put(&job_inputs_key(job_id), inputs)
            .await
            .map(|_| ())
            .map_err(|e| {
                tracing::warn!("[put_job_inputs({job_id})]: Error writing stream: {e:?}");
                e
            })
    }
}

#[async_trait]
impl SchedulerService for Scheduler {
    async fn get_status(&self) -> Result<SchedulerStatus> {
        let servers = {
            let mut servers = self.servers.lock().await;
            Self::prune_servers(&mut servers);

            let mut server_statuses = vec![];
            for (server_id, server) in servers.iter() {
                let u_time = server.m_time.elapsed().as_secs();
                server_statuses.push(ServerStatus {
                    id: server_id.clone(),
                    info: server.info.clone(),
                    jobs: server.jobs.clone(),
                    u_time,
                    max_job_age: if server.jobs.running == 0 {
                        server.max_job_age
                    } else {
                        server.max_job_age + u_time
                    },
                });
            }
            server_statuses
        };

        Ok(SchedulerStatus {
            info: Some(
                servers
                    .iter()
                    .fold(dist::SysStats::default(), |mut info, server| {
                        if server.info.cpu_usage.is_finite() {
                            info.cpu_usage += server.info.cpu_usage * server.info.num_cpus as f32;
                        }
                        info.mem_avail += server.info.mem_avail;
                        info.mem_total += server.info.mem_total;
                        info.num_cpus += server.info.num_cpus;
                        info.occupancy += server.info.occupancy;
                        info.pre_fetch += server.info.pre_fetch;
                        info
                    }),
            )
            .map(|mut info| {
                if !servers.is_empty() {
                    info.cpu_usage /= servers.iter().map(|s| s.info.num_cpus).sum::<usize>() as f32;
                }
                info
            })
            .unwrap(),
            jobs: dist::JobStats {
                accepted: servers
                    .iter()
                    .fold(0u64, |acc, server| acc.saturating_add(server.jobs.accepted)),
                finished: servers
                    .iter()
                    .fold(0u64, |acc, server| acc.saturating_add(server.jobs.finished)),
                loading: servers
                    .iter()
                    .fold(0u64, |acc, server| acc.saturating_add(server.jobs.loading)),
                pending: servers
                    .iter()
                    .fold(0u64, |acc, server| acc.saturating_add(server.jobs.pending)),
                running: servers
                    .iter()
                    .fold(0u64, |acc, server| acc.saturating_add(server.jobs.running)),
            },
            servers,
        })
    }

    async fn has_toolchain(&self, toolchain: &Toolchain) -> bool {
        self.toolchains.has(&toolchain.archive_id).await
    }

    async fn put_toolchain(
        &self,
        toolchain: &Toolchain,
        toolchain_archive: opendal::Buffer,
    ) -> Result<SubmitToolchainResult> {
        // Record put_toolchain time
        let _timer = self.metrics.put_toolchain_timer();
        // Upload toolchain to toolchains storage (S3, GCS, etc.)
        self.toolchains
            .put(&toolchain.archive_id, toolchain_archive)
            .await
            .map(|_| SubmitToolchainResult::Success)
            .context("Failed to put toolchain")
            .map_err(|err| {
                tracing::error!("[put_toolchain({})]: {err:?}", toolchain.archive_id);
                err
            })
    }

    async fn del_toolchain(&self, toolchain: &Toolchain) -> Result<()> {
        // Record del_toolchain time
        let _timer = self.metrics.del_toolchain_timer();
        // Delete the toolchain from toolchains storage (S3, GCS, etc.)
        self.toolchains
            .del(&toolchain.archive_id)
            .await
            .context("Failed to delete toolchain")
            .map_err(|err| {
                tracing::error!("[del_toolchain({})]: {err:?}", toolchain.archive_id);
                err
            })
    }

    async fn new_job(
        &self,
        toolchain: Toolchain,
        inputs: opendal::Buffer,
    ) -> Result<NewJobResponse> {
        let job_id = uuid::Uuid::new_v4().as_simple().to_string();
        let (has_toolchain, has_inputs) = futures::future::join(
            async { Ok::<bool, anyhow::Error>(self.has_toolchain(&toolchain).await) },
            async {
                self.put_job_inputs(&job_id, inputs).await.map_err(|err| {
                    // Record put_job_result errors after retrying
                    self.metrics.inc_put_job_inputs_error_count();
                    tracing::warn!("[new_job({job_id})]: Error storing job result: {err:?}");
                    err
                })
            },
        )
        .await;

        Ok(NewJobResponse {
            has_inputs: has_inputs.is_ok(),
            has_toolchain: has_toolchain.unwrap_or_default(),
            job_id,
            timeout: self.tasks.get_job_time_limit(),
        })
    }

    async fn put_job(&self, job_id: &str, inputs: opendal::Buffer) -> Result<()> {
        self.put_job_inputs(job_id, inputs).await
    }

    async fn run_job(
        &self,
        job_id: &str,
        RunJobRequestV2 {
            toolchain,
            command,
            outputs,
            ..
        }: RunJobRequestV2,
    ) -> Result<RunJobResponse> {
        if self.has_job_result(job_id).await {
            match self.get_job_result(job_id).await {
                Ok(RunJobResponse::FatalError { message, server_id }) => {
                    return Ok(RunJobResponse::FatalError { message, server_id });
                }
                Ok(RunJobResponse::Complete { result, server_id }) => {
                    return Ok(RunJobResponse::Complete { result, server_id });
                }
                // All others, delete the bad result and retry run_job
                _ => {
                    let _ = self.del_job_result(job_id).await;
                }
            }
        }

        if !self.has_toolchain(&toolchain).await {
            tracing::warn!(
                "[run_job({job_id})]: Missing toolchain {:?}",
                &toolchain.archive_id
            );
            return Ok(RunJobResponse::MissingToolchain {
                server_id: self.scheduler_id.clone(),
            });
        }

        if !self.has_job_inputs(job_id).await {
            tracing::warn!("[run_job({job_id})]: Missing job inputs");
            return Ok(RunJobResponse::MissingJobInputs {
                server_id: self.scheduler_id.clone(),
            });
        }

        self.run_job
            .call(RunJobArgs {
                job_id: job_id.to_owned(),
                reply_to: self.tasks.app().default_queue.clone(),
                toolchain,
                command,
                outputs,
            })
            .await
            .map(|(_, res)| res)
    }

    async fn has_job(&self, job_id: &str) -> bool {
        self.jobs.lock().await.contains_key(job_id)
    }

    async fn del_job(&self, job_id: &str) -> Result<()> {
        if self.has_job_inputs(job_id).await {
            let _ = self.del_job_inputs(job_id).await;
        }
        if self.has_job_result(job_id).await {
            let _ = self.del_job_result(job_id).await;
        }
        Ok(())
    }

    async fn job_finished(&self, job_id: &str, status: StatusUpdate) -> Result<()> {
        if let Some(sndr) = self.jobs.lock().await.remove(job_id) {
            let job_result = self.get_job_result(job_id).await.unwrap_or_else(|_| {
                RunJobResponse::MissingJobResult {
                    server_id: status.id.clone(),
                }
            });
            let job_success = matches!(job_result, RunJobResponse::Complete { .. });
            let job_success = sndr
                .send(job_result)
                .map_or_else(|_| false, |_| job_success);
            self.update_server_status(status, Some(job_success)).await
        } else {
            Err(anyhow!("Unknown job {job_id:?}"))
        }
    }

    async fn update_server_status(
        &self,
        status: StatusUpdate,
        job_status: Option<bool>,
    ) -> Result<()> {
        if let Some(success) = job_status {
            if success {
                tracing::trace!("Received server success: {status:?}");
            } else {
                tracing::trace!("Received server failure: {status:?}");
            }
        }

        let mut servers = self.servers.lock().await;

        fn duration_from_micros((secs, nanos): (u64, u32)) -> Duration {
            Duration::new(secs, nanos)
        }

        // Insert or update the server info
        servers
            .entry(status.id.clone())
            .and_modify(|server| {
                // Convert to absolute durations since the Unix epoch
                let t1 = server.u_time.duration_since(UNIX_EPOCH).unwrap();
                let t2 = duration_from_micros(status.timestamp);
                // If this event is newer than the latest state, it is now the latest state.
                if t2 >= t1 {
                    server.info = status.info.clone();
                    server.jobs = status.jobs.clone();
                    server.queue = status.queue.clone();
                    server.m_time = Instant::now();
                    server.max_job_age = duration_from_micros(status.max_job_age).as_secs();
                    // Increment prev time with the difference between prev and next
                    server.u_time = server.u_time.checked_add(t2 - t1).unwrap();
                }
            })
            .or_insert_with(|| ServerInfo {
                info: status.info,
                jobs: status.jobs,
                queue: status.queue,
                m_time: Instant::now(),
                max_job_age: duration_from_micros(status.max_job_age).as_secs(),
                // Convert to absolute duration since the Unix epoch
                u_time: UNIX_EPOCH
                    .checked_add(duration_from_micros(status.timestamp))
                    .unwrap(),
            });

        Self::prune_servers(&mut servers);

        Ok(())
    }
}
