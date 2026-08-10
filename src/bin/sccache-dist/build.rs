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

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bytes::Buf;
use fs_err as fs;
use futures::lock::Mutex;
use itertools::Itertools;
use libmount::Overlay;
use sccache::dist::{
    BuildError, BuildResult as BuildOutput, BuilderIncoming, CompileCommand, OutputData,
};
use sccache::mock_command::ProcessOutput;
use std::collections::HashMap;
use std::io;
use std::path::{self, Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::ChildStdin;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use version_compare::Version;

pub type BuildResult = std::result::Result<BuildOutput, BuildError>;

#[async_trait]
trait AsyncCommandExt {
    async fn check_stdout_trim(&mut self) -> Result<String>;
    async fn check_piped<F, Fut>(&mut self, pipe: F) -> Result<()>
    where
        F: FnOnce(ChildStdin) -> Fut + std::marker::Send,
        Fut: std::future::Future<Output = Result<()>> + std::marker::Send;
    async fn check_run(&mut self) -> Result<()>;
}

#[async_trait]
impl AsyncCommandExt for tokio::process::Command {
    async fn check_stdout_trim(&mut self) -> Result<String> {
        let output = self.output().await.context("Failed to start command")?;
        check_output(&output)?;
        let stdout =
            String::from_utf8(output.stdout).context("Output from listing containers not UTF8")?;
        Ok(stdout.trim().to_owned())
    }
    async fn check_piped<F, Fut>(&mut self, pipe: F) -> Result<()>
    where
        F: FnOnce(ChildStdin) -> Fut + std::marker::Send,
        Fut: std::future::Future<Output = Result<()>> + std::marker::Send,
    {
        let mut process = self
            .stdin(Stdio::piped())
            .spawn()
            .context("Failed to start command")?;
        pipe(
            process
                .stdin
                .take()
                .expect("Requested piped stdin but not present"),
        )
        .await
        .context("Failed to pipe input to process")?;
        let output = process
            .wait_with_output()
            .await
            .context("Failed to wait for process to return")?;
        check_output(&output)
    }
    async fn check_run(&mut self) -> Result<()> {
        let output = self.output().await.context("Failed to start command")?;
        check_output(&output)
    }
}

fn check_output(output: &Output) -> Result<()> {
    if !output.status.success() {
        tracing::warn!(
            "\n\n===========\n{}\n==========\n\n=========\n{}\n===============\n\n",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        bail!("Command failed with status {}", output.status)
    }
    Ok(())
}

fn join_suffix<P: AsRef<Path>>(path: &Path, suffix: P) -> PathBuf {
    let suffixpath = suffix.as_ref();
    let mut components = suffixpath.components();
    if suffixpath.has_root() {
        assert_eq!(components.next(), Some(path::Component::RootDir));
    }
    path.join(components)
}

#[derive(Clone, Debug)]
struct OverlaySpec {
    build_dir: PathBuf,
    toolchain_dir: PathBuf,
}

type OverlayChildren = Mutex<
    HashMap<
        String,
        (
            OverlaySpec,
            tokio::sync::oneshot::Sender<()>,
            tokio::sync::oneshot::Receiver<()>,
        ),
    >,
>;

#[derive(Clone)]
pub struct OverlayBuilder {
    bubblewrap: PathBuf,
    children: Arc<OverlayChildren>,
    dir: PathBuf,
    job_queue: Arc<tokio::sync::Semaphore>,
}

impl OverlayBuilder {
    pub async fn new(
        bubblewrap: PathBuf,
        build_dir: PathBuf,
        job_queue: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self> {
        tracing::info!("Creating overlay builder with dir {build_dir:?}");

        if !nix::unistd::getuid().is_root() && !nix::unistd::geteuid().is_root() {
            // Not root, or a setuid binary - haven't put enough thought into supporting this, bail
            bail!("not running as root")
        }

        let mut cmd = tokio::process::Command::new(&bubblewrap);

        let out = cmd
            .arg("--version")
            .check_stdout_trim()
            .await
            .context("Failed to execute bwrap for version check")?;

        if let Some(s) = out.split_whitespace().nth(1) {
            match (Version::from("0.3.0"), Version::from(s)) {
                (Some(min), Some(seen)) => {
                    if seen < min {
                        bail!(
                            "bubblewrap 0.3.0 or later is required, got {out:?} for {bubblewrap:?}"
                        );
                    }
                }
                (_, _) => {
                    bail!(
                        "Unexpected version format running {bubblewrap:?}: got {out:?}, expected \"bubblewrap x.x.x\""
                    );
                }
            }
        } else {
            bail!(
                "Unexpected version format running {bubblewrap:?}: got {out:?}, expected \"bubblewrap x.x.x\""
            );
        }

        let dir = build_dir.join("builds");

        // TODO: pidfile
        let ret = Self {
            bubblewrap,
            children: Default::default(),
            dir,
            job_queue,
        };
        ret.cleanup().await?;
        tokio::fs::create_dir_all(&ret.dir)
            .await
            .context("Failed to create builder builds directory")?;
        Ok(ret)
    }

    async fn cleanup(&self) -> Result<()> {
        if self.dir.exists() {
            tokio::fs::remove_dir_all(&self.dir)
                .await
                .context("Failed to clean up builder directory")?;
        }
        Ok(())
    }

    async fn prepare_overlay_dirs(
        &self,
        job_id: &str,
        toolchain_dir: &Path,
    ) -> Result<OverlaySpec> {
        let build_dir = self.dir.join(uuid::Uuid::new_v4().as_simple().to_string());

        tracing::trace!(
            "[prepare_overlay_dirs({job_id})]: Creating build directory: {build_dir:?}"
        );

        tokio::fs::create_dir_all(&build_dir)
            .await
            .context("Failed to create build dir")
            .unwrap_or_else(|err| tracing::warn!("[prepare_overlay_dirs({job_id})]: {err:?}"));

        Ok(OverlaySpec {
            build_dir,
            toolchain_dir: toolchain_dir.to_owned(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn perform_build(
        job_id: &str,
        bubblewrap: PathBuf,
        CompileCommand {
            executable,
            arguments,
            env_vars,
            cwd,
        }: CompileCommand,
        inputs: opendal::Buffer,
        output_paths: Vec<String>,
        overlay: OverlaySpec,
        job_queue: &tokio::sync::Semaphore,
        children: &OverlayChildren,
    ) -> BuildResult {
        tracing::trace!("[perform_build({job_id})]: Compile environment: {env_vars:?}");
        tracing::trace!("[perform_build({job_id})]: Compile command: {executable:?} {arguments:?}");
        tracing::trace!("[perform_build({job_id})]: Output paths: {output_paths:?}");

        if output_paths.is_empty() {
            return Err(BuildError::Unknown(anyhow!("Output paths is empty")));
        }

        let job_id_1 = job_id.to_owned();
        let overlay_1 = overlay.clone();

        let build_in_overlay =
            move |runtime: &tokio::runtime::Handle,
                  cancelled_tx: tokio::sync::oneshot::Sender<()>,
                  cancelled_rx: tokio::sync::oneshot::Receiver<()>| {
                let job_id = job_id_1.clone();
                let job_id_1 = job_id.clone();
                let build_dir = overlay_1.build_dir;
                let toolchain_dir = overlay_1.toolchain_dir;

                // Automatically unmount or destroy file system mounts when this thread dies
                nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS)
                    .map_err(BuildError::EnterNewNS)?;
                // Turn / into a 'slave', so it receives mounts from real root, but doesn't propagate back
                nix::mount::mount(
                    Option::<&str>::None,
                    "/",
                    Option::<&str>::None,
                    // Make all future mount changes private to this namespace
                    nix::mount::MsFlags::MS_REC | nix::mount::MsFlags::MS_PRIVATE,
                    Option::<&str>::None,
                )
                .map_err(BuildError::MountNsRoot)?;

                let work_dir = build_dir.join("work");
                let upper_dir = build_dir.join("upper");
                let target_dir = build_dir.join("target");
                fs::create_dir_all(&work_dir)
                    .map_err(|e| BuildError::MakeOverlayDir(work_dir.clone(), e))?;
                fs::create_dir_all(&upper_dir)
                    .map_err(|e| BuildError::MakeOverlayDir(upper_dir.clone(), e))?;
                fs::create_dir_all(&target_dir)
                    .map_err(|e| BuildError::MakeOverlayDir(target_dir.clone(), e))?;

                let () = Overlay::writable(
                    std::iter::once(toolchain_dir.as_path()),
                    upper_dir,
                    work_dir,
                    &target_dir,
                )
                .mount()
                // This error is unfortunately not Send+Sync
                .map_err(|e| BuildError::MountOverlayFS(anyhow!("{e}")))?;

                tracing::trace!("[perform_build({job_id})]: copying in inputs");
                // Note that we don't unpack directly into the upperdir since there overlayfs has some
                // special marker files that we don't want to create by accident (or malicious intent)
                tar::Archive::new(flate2::read::ZlibDecoder::new(inputs.reader()))
                    .unpack(&target_dir)
                    .context("Failed to unpack inputs to overlay")
                    .map_err(BuildError::UnpackInputs)?;

                let cwd = Path::new(&cwd);

                tracing::trace!("[perform_build({job_id})]: creating output directories");

                // Canonicalize output path as either absolute or relative to cwd
                let output_paths_absolute = output_paths
                    .iter()
                    .map(|path| cwd.join(Path::new(path)))
                    .collect::<Vec<_>>();

                {
                    let h_cwd = join_suffix(&target_dir, cwd);
                    tracing::trace!("[perform_build({job_id})]: creating dir: {h_cwd:?}");
                    fs::create_dir_all(&h_cwd).map_err(|e| BuildError::MakeOutputDir(h_cwd, e))?;
                }

                for path in output_paths_absolute.iter() {
                    // If it doesn't have a parent, nothing needs creating
                    if let Some(path) = path.parent() {
                        let h_path = join_suffix(&target_dir, path);
                        tracing::trace!("[perform_build({job_id})]: creating dir: {h_path:?}");
                        fs::create_dir_all(&h_path)
                            .map_err(|e| BuildError::MakeOutputDir(h_path, e))?;
                    }
                }

                tracing::trace!("[perform_build({job_id})]: creating compile command");

                // Bubblewrap notes:
                // - We're running as uid 0 (to do the mounts above), and so bubblewrap is run as uid 0
                // - There's special handling in bubblewrap to compare uid and euid - of interest to us,
                //   if uid == euid == 0, bubblewrap preserves capabilities (not good!) so we explicitly
                //   drop all capabilities
                // - By entering a new user namespace means any set of capabilities do not apply to any
                //   other user namespace, i.e. you lose privileges. This is not strictly necessary because
                //   we're dropping caps anyway so it's irrelevant which namespace we're in, but it doesn't
                //   hurt.
                // - --unshare-all is not ideal as it happily continues if it fails to unshare either
                //   the user or cgroups namespace, so we list everything explicitly
                // - The order of bind vs proc + dev is important - the new root must be put in place
                //   first, otherwise proc and dev get hidden
                let mut cmd = tokio::process::Command::new(bubblewrap);
                cmd.arg("--die-with-parent")
                    .args(["--cap-drop", "ALL"])
                    .args([
                        "--unshare-user",
                        "--unshare-cgroup",
                        "--unshare-ipc",
                        "--unshare-pid",
                        "--unshare-net",
                        "--unshare-uts",
                    ])
                    .arg("--bind")
                    .arg(&target_dir)
                    .arg("/")
                    .args(["--proc", "/proc"])
                    .args(["--dev", "/dev"])
                    .arg("--chdir")
                    .arg(cwd);

                for (k, v) in env_vars {
                    if k.contains('=') {
                        tracing::warn!(
                            "[perform_build({job_id})]: Skipping environment variable: {k:?}"
                        );
                        continue;
                    }
                    cmd.arg("--setenv").arg(k).arg(v);
                }
                cmd.arg("--");
                cmd.arg(&executable);
                cmd.args(arguments);
                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());
                cmd.kill_on_drop(true);

                tracing::trace!("[perform_build({job_id})]: performing compile");
                tracing::trace!("[perform_build({job_id})]: bubblewrap command: {:?}", cmd);

                let output = runtime.block_on(async move {
                    let mut child = match cmd.spawn() {
                        Ok(child) => child,
                        Err(e) => return Some(Err(BuildError::SpawnChildProcess(e))),
                    };

                    async fn buffer_output<S>(stream: Option<S>) -> std::result::Result<Vec<u8>, BuildError>
                    where
                        S: AsyncReadExt + Unpin,
                    {
                        let mut buf = Vec::new();
                        if let Some(mut stream) = stream {
                            stream
                                .read_to_end(&mut buf)
                                .await
                                .map_err(BuildError::ReadChildOutput)?;
                        }
                        std::result::Result::<Vec<u8>, BuildError>::Ok(buf)
                    }

                    let stdout = buffer_output(child.stdout.take());
                    let stderr = buffer_output(child.stderr.take());
                    let status = {
                        let status = child.wait();
                        async move { status.await.map_err(BuildError::WaitChildOutput) }
                    };

                    let completed = async move {
                        futures::future::try_join3(status, stdout, stderr)
                            .await
                            .map(|(status, stdout, stderr)| {
                                ProcessOutput::from(std::process::Output {
                                    status,
                                    stdout,
                                    stderr,
                                })
                            })
                    };

                    tokio::select! {
                        biased;
                        output = completed => {
                            Some(output)
                        },
                        _ = cancelled_rx => {
                            let output = if let Err(err) = child.kill().await.map_err(BuildError::KillChildProcess) {
                                tracing::warn!("[perform_build({job_id_1})]: {err:?}");
                                Some(Err(err))
                            } else {
                                tracing::debug!("[perform_build({job_id_1})]: Build process killed");
                                None
                            };
                            let _ = cancelled_tx.send(());
                            output
                        },
                    }
                });

                let output = match output {
                    None => return Err(BuildError::Cancelled),
                    Some(output) => output?,
                };

                let mut outputs = vec![];

                if !output.success() {
                    if output.exit() {
                        tracing::trace!("[perform_build({job_id})]: compile failure: {output:?}");
                    } else {
                        // Warn on abnormal terminations (i.e. SIGTERM, SIGKILL)
                        tracing::warn!(
                            "[perform_build({job_id})]: {executable:?} {}",
                            output.desc()
                        );
                    }
                } else {
                    tracing::trace!("[perform_build({job_id})]: compile success: {output:?}");
                    tracing::trace!("[perform_build({job_id})]: retrieving {output_paths:?}");

                    for (path, abs_path) in output_paths.into_iter().zip(output_paths_absolute) {
                        let host_path = join_suffix(&target_dir, &abs_path);
                        match fs::File::open(&host_path) {
                            Ok(file) => match OutputData::try_from_reader(file) {
                                Ok(data) => outputs.push((path, data)),
                                Err(err) => {
                                    tracing::error!(
                                        "[perform_build({job_id})]: Failed to read and compress output file host={host_path:?}, overlay={abs_path:?}: {err}"
                                    );
                                }
                            },
                            Err(err) => {
                                if err.kind() == io::ErrorKind::NotFound {
                                    tracing::debug!(
                                        "[perform_build({job_id})]: Missing output path host={host_path:?}, overlay={abs_path:?}"
                                    );
                                } else {
                                    return Err(BuildError::ReadBuildResult(abs_path, err));
                                }
                            }
                        }
                    }
                }

                BuildResult::Ok(BuildOutput { output, outputs })
            };

        let runtime = tokio::runtime::Handle::current();
        let (cancel_job_tx, cancel_job_rx) = tokio::sync::oneshot::channel();
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
        let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();

        children
            .lock()
            .await
            .insert(job_id.to_owned(), (overlay, cancel_job_tx, cancelled_rx));

        {
            // Guard compiling until we get a token from the job queue
            let _job_slot = job_queue
                .acquire()
                .await
                .map_err(|e| BuildError::Unknown(e.into()))?;

            // Explicitly launch a new thread outside tokio's thread pool,
            // so that our overlayfs and tmpfs are unmounted when it dies.
            //
            // Keeping the handle alive for the lifetime of the oneshot channel
            // to ensure it's only dropped if the Future is cancelled.
            let handle = std::thread::spawn(move || {
                completed_tx.send(build_in_overlay(&runtime, cancelled_tx, cancel_job_rx))
            });

            // Asynchronously wait till the build thread is done so we don't block the tokio worker thread.
            let res = completed_rx
                .await
                .map_err(|e| BuildError::Unknown(anyhow!(e)))?;

            // Now that the build thread is done, join() will return immediately.
            handle
                .join()
                // Handle panics
                .map_err(|e| BuildError::Unknown(anyhow!("{e:?}")))
                // Handle unwrapping if completed_tx.send() returns Err
                .and_then(|out| out.map(|_| res).unwrap_or_else(|out| out))
        }
    }

    async fn finish_overlay(job_id: &str, overlay: &OverlaySpec) {
        let OverlaySpec {
            build_dir,
            toolchain_dir: _,
        } = overlay;

        if build_dir.exists()
            && let Err(e) = tokio::fs::remove_dir_all(build_dir).await
        {
            tracing::warn!(
                "[finish_overlay({job_id})]: Failed to remove build directory {:?}: {e:?}",
                build_dir.display()
            );
        }
    }
}

#[async_trait]
impl BuilderIncoming for OverlayBuilder {
    async fn run_build(
        &self,
        job_id: &str,
        toolchain_dir: &Path,
        inputs: opendal::Buffer,
        command: CompileCommand,
        outputs: Vec<String>,
    ) -> BuildResult {
        // Bail early if job_queue is closed while this job is running
        if self.job_queue.is_closed() {
            return Err(BuildError::Cancelled);
        }

        tracing::debug!("[run_build({job_id})]: Preparing overlay");

        let overlay = self
            .prepare_overlay_dirs(job_id, toolchain_dir)
            .await
            .map_err(BuildError::PrepareOverlay)?;

        tracing::debug!("[run_build({job_id})]: Performing build in {overlay:?}");

        let res = Self::perform_build(
            job_id,
            self.bubblewrap.clone(),
            command,
            inputs,
            outputs,
            overlay.clone(),
            self.job_queue.as_ref(),
            self.children.as_ref(),
        )
        .await;

        tracing::debug!("[run_build({job_id})]: Returning result");

        res
    }

    async fn finish_build(&self, job_id: &str) {
        if let Some((overlay, cancel, cancelled)) = self.children.lock().await.remove(job_id) {
            tracing::debug!(
                "[finish_build({job_id})]: Finishing with overlay {:?}",
                overlay.build_dir.display()
            );
            let _ = futures::join!(async { cancel.send(()) }, cancelled);
            Self::finish_overlay(job_id, &overlay).await;
        }
    }

    async fn shutdown(&self) {
        self.job_queue.close();
        futures::future::join_all(self.children.lock().await.drain().map(
            |(job_id, (overlay, cancel, cancelled))| async move {
                tracing::debug!(
                    "[shutdown({job_id})]: Finishing with overlay {:?}",
                    overlay.build_dir.display()
                );
                let _ = futures::join!(async { cancel.send(()) }, cancelled);
                Self::finish_overlay(&job_id, &overlay).await;
            },
        ))
        .await;
    }
}

#[derive(Clone)]
pub struct DockerBuilder {
    image: String,
    run_cmd: Vec<String>,
    exec_cmd: Vec<String>,
    containers: Arc<Mutex<HashMap<String, String>>>,
    job_queue: Arc<tokio::sync::Semaphore>,
}

impl DockerBuilder {
    // TODO: this should accept a unique string, e.g. inode of the tccache directory
    // having locked a pidfile, or at minimum should loudly detect other running
    // instances - pidfile in /tmp
    pub async fn new(
        image: String,
        run_cmd: Vec<String>,
        exec_cmd: Vec<String>,
        job_queue: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self> {
        tracing::info!(
            "Creating docker builder image={image:?}, run_cmd={run_cmd:?}, exec_cmd={exec_cmd:?}"
        );
        Ok(Self {
            image,
            run_cmd,
            exec_cmd,
            containers: Default::default(),
            job_queue,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn perform_build(
        job_id: &str,
        c_name: &str,
        image: &str,
        run_cmd: &[String],
        exec_cmd: &[String],
        toolchain_tgz: &Path,
        CompileCommand {
            executable,
            arguments,
            env_vars,
            cwd,
        }: CompileCommand,
        output_paths: Vec<String>,
        inputs: opendal::Buffer,
        job_queue: &tokio::sync::Semaphore,
    ) -> BuildResult {
        tracing::trace!("[perform_build({job_id})]: Compile environment: {env_vars:?}");
        tracing::trace!("[perform_build({job_id})]: Compile command: {executable:?} {arguments:?}");
        tracing::trace!("[perform_build({job_id})]: Output paths: {output_paths:?}");

        if output_paths.is_empty() {
            return Err(BuildError::Unknown(anyhow!("Output paths is empty")));
        }

        // Do as much asyncio work as possible before acquiring a job slot

        // Should automatically get deleted when host_temp goes out of scope
        let host_temp = sccache::util::temp_dir().map_err(BuildError::Unknown)?;
        let host_root = host_temp.path();

        let cwd = Path::new(&cwd);

        // Canonicalize output path as either absolute or relative to cwd
        let container_absolute_output_paths = output_paths
            .iter()
            .map(|path| cwd.join(Path::new(path)))
            .collect::<Vec<_>>();

        // Collect cwd and output dir paths
        // Always create the CWD even if it's not in the inputs archive
        let container_absolute_output_dirs = container_absolute_output_paths
            .iter()
            // If it doesn't have a parent, nothing needs creating
            .filter_map(|p| p.parent())
            .unique()
            .collect::<Vec<_>>();

        {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            tracing::trace!("[perform_build({job_id})]: creating docker container");

            let mut cmd = tokio::process::Command::new("docker");
            cmd.args(["run", "--init", "-d", "--name", c_name])
                .arg(image)
                .args(run_cmd)
                .check_stdout_trim()
                .await
                .context("Failed to create docker container")
                .map_err(BuildError::Unknown)?;
        }

        {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            tracing::trace!(
                "[perform_build({job_id})]: creating output directories: {container_absolute_output_dirs:?}"
            );

            let mut cmd = tokio::process::Command::new("docker");
            cmd.arg("exec")
                .args(["--workdir", "/", c_name, "mkdir", "-p"])
                // Always create the CWD even if it's not in the toolchain, inputs, or outputs
                .arg(cwd)
                .args(&container_absolute_output_dirs)
                .check_run()
                .await
                .context("Failed to copy toolchain into container")
                .map_err(BuildError::UnpackInputs)?;
        }

        {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            tracing::trace!("[perform_build({job_id})]: copying in inputs");

            let mut cmd = tokio::process::Command::new("docker");
            cmd.arg("exec")
                .args([
                    "--interactive",
                    "--workdir",
                    "/",
                    c_name,
                    "tar",
                    "--keep-directory-symlink",
                    "-xf",
                    "-",
                    "-C",
                    "/",
                ])
                .check_piped(|mut stdin| async move {
                    let inputs = async_compression::futures::bufread::ZlibDecoder::new(
                        futures::io::BufReader::new(futures::io::AllowStdIo::new(inputs.reader())),
                    );
                    tokio::io::copy(&mut inputs.compat(), &mut stdin).await?;
                    Ok(())
                })
                .await
                .context("Failed to copy inputs into container")
                .map_err(BuildError::UnpackInputs)?;
        }

        {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            tracing::trace!("[perform_build({job_id})]: copying in toolchain");

            let mut cmd = tokio::process::Command::new("docker");
            cmd.arg("exec")
                .args([
                    "--interactive",
                    "--workdir",
                    "/",
                    c_name,
                    "tar",
                    "--keep-directory-symlink",
                    "-xf",
                    "-",
                    "-C",
                    "/",
                ])
                .check_piped(|mut stdin| async move {
                    tracing::trace!(
                        "[perform_build({job_id})]: opening compressed toolchain {toolchain_tgz:?}"
                    );

                    let toolchain =
                        tokio::fs::File::open(&toolchain_tgz)
                            .await
                            .with_context(|| {
                                format!("Failed to open compressed toolchain {toolchain_tgz:?}")
                            })?;

                    let toolchain = async_compression::futures::bufread::GzipDecoder::new(
                        futures::io::BufReader::new(toolchain.compat()),
                    );

                    tokio::io::copy(&mut toolchain.compat(), &mut stdin).await?;
                    Ok(())
                })
                .await
                .context("Failed to copy toolchain into container")
                .map_err(BuildError::Unknown)?;
        }

        let output: ProcessOutput = {
            // Guard compiling until we get a token from the job queue
            let _job_slot = job_queue
                .acquire()
                .await
                .map_err(|e| BuildError::Unknown(e.into()))?;

            tracing::trace!("[perform_build({job_id})]: creating compile command");

            // TODO: likely shouldn't perform the compile as root in the container
            let mut cmd = tokio::process::Command::new("docker");

            cmd.arg("exec")
                // Run in `cwd`
                .arg("--workdir")
                .arg(cwd)
                // Define envvars
                .args(env_vars.iter().flat_map(|(k, v)| {
                    if k.contains('=') {
                        tracing::warn!(
                            "[perform_build({job_id})]: Skipping environment variable: {k:?}"
                        );
                        vec![]
                    } else {
                        vec!["--env".into(), format!("{k}=\"{v}\"")]
                    }
                }))
                // container name
                .arg(c_name)
                // The exec command (e.g. /usr/bin/wine)
                .args(exec_cmd)
                // Finally, the executable and arguments
                .arg(&executable)
                .args(arguments);

            tracing::trace!("[perform_build({job_id})]: performing compile");
            tracing::trace!("[perform_build({job_id})]: {:?}", cmd.as_std());

            cmd.output()
                .await
                .map_err(BuildError::SpawnChildProcess)?
                .into()
        };

        let outputs = {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            let mut outputs = vec![];

            if !output.success() {
                if output.exit() {
                    tracing::trace!("[perform_build({job_id})]: compile failure: {output:?}");
                } else {
                    // Warn on abnormal terminations (i.e. SIGTERM, SIGKILL)
                    tracing::warn!(
                        "[perform_build({job_id})]: {executable:?} terminated with {}",
                        output.desc()
                    );
                }
            } else {
                tracing::trace!("[perform_build({job_id})]: compile success: {output:?}");
                tracing::trace!("[perform_build({job_id})]: retrieving {output_paths:?}");

                let host_absolute_output_dirs = container_absolute_output_dirs
                    .iter()
                    // Resolve in case it's relative since we copy it from the root level
                    .map(|abs_path| join_suffix(host_root, abs_path))
                    .collect::<Vec<_>>();

                // Create the parent of each output dir on the host
                for host_path in host_absolute_output_dirs
                    .iter()
                    .filter_map(|p| p.parent())
                    .filter(|p| !p.exists())
                {
                    tracing::trace!("[perform_build({job_id})]: creating host dir: {host_path:?}");
                    tokio::fs::create_dir_all(&host_path)
                        .await
                        .map_err(|e| BuildError::MakeOutputDir(host_path.to_path_buf(), e))?;
                }

                // Copy the output dirs from the container to host
                for (container_path, host_path) in container_absolute_output_dirs
                    .into_iter()
                    .zip(host_absolute_output_dirs)
                {
                    tracing::trace!(
                        "[perform_build({job_id})]: copying output dir: {container_path:?} -> {host_path:?}"
                    );

                    let mut cmd = tokio::process::Command::new("docker");
                    cmd.arg("cp")
                        .arg("--follow-link")
                        .arg(format!("{c_name}:{}", container_path.display()))
                        .arg(host_path)
                        .check_run()
                        .await
                        .context("Failed to copy output directory from container")
                        .map_err(|err| BuildError::CopyOutputDir(container_path.to_owned(), err))?;
                }

                // Tar up the output files
                for (output_path, output_path_abs) in output_paths
                    .into_iter()
                    .zip(container_absolute_output_paths)
                {
                    // Resolve in case it's relative since we copy it from the root level
                    let host_path = join_suffix(host_root, &output_path_abs);

                    match fs::File::open(&host_path) {
                        Ok(file) => match OutputData::try_from_reader(file) {
                            Ok(data) => outputs.push((output_path, data)),
                            Err(err) => {
                                tracing::error!(
                                    "[perform_build({job_id})]: Failed to read and compress output file host={host_path:?}, container={output_path:?}: {err}"
                                );
                            }
                        },
                        Err(err) => {
                            if err.kind() == io::ErrorKind::NotFound {
                                tracing::debug!(
                                    "[perform_build({job_id})]: Missing output path host={host_path:?}, container={output_path:?}"
                                );
                            } else {
                                return Err(BuildError::ReadBuildResult(output_path_abs, err));
                            }
                        }
                    }
                }
            }

            outputs
        };

        BuildResult::Ok(BuildOutput { output, outputs })
    }

    async fn finish_container(job_id: &str, c_name: &str) {
        if let Err(err) = tokio::process::Command::new("docker")
            .args(["rm", "-f", c_name])
            .check_run()
            .await
        {
            tracing::warn!(
                "[finish_container({job_id})]: Failed to remove docker container {c_name:?}: {err:#}",
            );
        }
    }
}

#[async_trait]
impl BuilderIncoming for DockerBuilder {
    // From Server
    async fn run_build(
        &self,
        job_id: &str,
        toolchain_tgz: &Path,
        inputs: opendal::Buffer,
        command: CompileCommand,
        outputs: Vec<String>,
    ) -> BuildResult {
        // Bail early if job_queue is closed while this job is running
        if self.job_queue.is_closed() {
            return Err(BuildError::Cancelled);
        }

        tracing::debug!("[run_build({job_id})]: Performing build in container");
        let c_name = format!("sccache-builder-{job_id}");

        self.containers
            .lock()
            .await
            .insert(job_id.to_owned(), c_name.clone());

        let res = Self::perform_build(
            job_id,
            &c_name,
            &self.image,
            &self.run_cmd,
            &self.exec_cmd,
            toolchain_tgz,
            command,
            outputs,
            inputs,
            self.job_queue.as_ref(),
        )
        .await;

        tracing::debug!("[run_build({job_id})]: Returning result");

        res
    }

    async fn finish_build(&self, job_id: &str) {
        if let Some(c_name) = self.containers.lock().await.remove(job_id) {
            tracing::debug!("[finish_build({job_id})]: Removing container {c_name:?}");
            Self::finish_container(job_id, &c_name).await;
        }
    }

    async fn shutdown(&self) {
        self.job_queue.close();
        futures::future::join_all(self.containers.lock().await.drain().map(
            |(job_id, c_name)| async move {
                tracing::debug!("[shutdown({job_id})]: Removing container {c_name:?}");
                Self::finish_container(&job_id, &c_name).await;
            },
        ))
        .await;
    }
}
