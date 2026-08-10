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
use futures::lock::Mutex;
use itertools::Itertools;
use sccache::dist::{
    BuildError, BuildResult as BuildOutput, BuilderIncoming, CompileCommand, OutputData,
};
use sccache::mock_command::ProcessOutput;
use std::collections::{HashMap, hash_map};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use uuid::Uuid;

pub type BuildResult = std::result::Result<BuildOutput, BuildError>;

#[async_trait]
trait AsyncCommandExt {
    async fn check_stdout_trim(&mut self) -> Result<String>;
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
    async fn check_run(&mut self) -> Result<()> {
        let output = self.output().await.context("Failed to start command")?;
        check_output(&output)
    }
}

fn check_output(output: &Output) -> Result<()> {
    if !output.status.success() {
        tracing::warn!(
            "===========\n{}\n==========\n\n\n\n=========\n{}\n===============\n\n\n",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        bail!("Command failed with status {}", output.status)
    }
    Ok(())
}

fn list_dirs(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .same_file_system(true)
        .into_iter()
        .flatten()
        .filter_map(|entr| {
            entr.metadata()
                .ok()
                .and_then(|meta| meta.is_dir().then_some(entr))
        })
        .map(|entr| entr.path().to_path_buf())
        .collect::<Vec<_>>()
}

fn list_files(root: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .follow_links(false)
        .same_file_system(true)
        .into_iter()
        .flatten()
        // Only copy files and symlinks, not dirs
        .filter_map(|entr| {
            entr.metadata()
                .ok()
                .and_then(|meta| (!meta.is_dir()).then_some(entr))
        })
        .map(|entr| entr.path().to_path_buf())
        .collect::<Vec<_>>()
}

// Force remove the container
async fn pot_rm(cid: &str, pot_cmd: &Path) -> Result<()> {
    let mut cmd = tokio::process::Command::new(pot_cmd);
    cmd.args(["destroy", "-F", "-p", cid])
        .check_run()
        .await
        .context("Failed to force delete container")
}

#[derive(Clone)]
pub struct PotBuilder {
    pot_fs_root: PathBuf,
    clone_from: String,
    pot_cmd: PathBuf,
    pot_clone_args: Vec<String>,
    image_map: Arc<Mutex<HashMap<PathBuf, String>>>,
    container_lists: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
    job_queue: Arc<tokio::sync::Semaphore>,
    cids: Arc<Mutex<HashMap<String, (String, PathBuf)>>>,
}

impl PotBuilder {
    pub async fn new(
        pot_fs_root: PathBuf,
        clone_from: String,
        pot_cmd: PathBuf,
        pot_clone_args: Vec<String>,
        job_queue: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self> {
        tracing::info!("Creating pot builder");

        let ret = Self {
            pot_fs_root,
            clone_from,
            pot_cmd,
            pot_clone_args,
            image_map: Arc::new(Mutex::new(HashMap::new())),
            container_lists: Arc::new(Mutex::new(HashMap::new())),
            job_queue,
            cids: Default::default(),
        };
        ret.cleanup().await?;
        Ok(ret)
    }

    // This removes all leftover pots from previous runs
    async fn cleanup(&self) -> Result<()> {
        tracing::info!("Performing initial pot cleanup");
        let mut cmd = tokio::process::Command::new(&self.pot_cmd);
        let mut to_remove = cmd
            .args(["ls", "-q"])
            .check_stdout_trim()
            .await
            .context("Failed to force delete container")?
            .split('\n')
            .filter(|a| a.starts_with("sccache-builder-") || a.starts_with("sccache-image-"))
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        to_remove.sort();
        for cid in to_remove {
            tracing::trace!("Removing pot {cid}");
            if let Err(e) = pot_rm(&cid, &self.pot_cmd).await {
                tracing::warn!("Failed to remove container {cid}: {e}");
            }
        }
        tracing::info!("Completed initial pot cleanup");
        Ok(())
    }

    // If we have a spare running container, claim it and remove it from the available list,
    // otherwise try and create a new container (possibly creating the Pot image along
    // the way)
    async fn get_container(&self, job_id: &str, toolchain_dir: &Path) -> Result<String> {
        let container = {
            let mut map = self.container_lists.lock().await;
            map.entry(toolchain_dir.to_path_buf())
                .or_insert_with(Vec::new)
                .pop()
        };
        match container {
            Some(cid) => Ok(cid),
            None => {
                // TODO: can improve parallelism (of creating multiple images at a time) by using another
                // (more fine-grained) mutex around the entry value and checking if its empty a second time
                let image = {
                    let mut map = self.image_map.lock().await;
                    match map.entry(toolchain_dir.to_path_buf()) {
                        hash_map::Entry::Occupied(e) => e.get().clone(),
                        hash_map::Entry::Vacant(e) => {
                            tracing::info!(
                                "[get_container({job_id})]: Creating pot image for {:?} (may block requests)",
                                toolchain_dir.components().next_back().unwrap()
                            );
                            let image = Self::make_image(
                                job_id,
                                toolchain_dir,
                                &self.pot_fs_root,
                                &self.clone_from,
                                &self.pot_cmd,
                                &self.pot_clone_args,
                            )
                            .await?;
                            e.insert(image.clone());
                            image
                        }
                    }
                };
                Self::start_container(&image, &self.pot_cmd, &self.pot_clone_args).await
            }
        }
    }

    async fn clean_container(cid: &str) -> Result<()> {
        let mut cmd = tokio::process::Command::new("pot");
        cmd.args(["stop", "-p", cid])
            .check_run()
            .await
            .context("Failed to stop container")?;

        let mut cmd = tokio::process::Command::new("pot");
        cmd.args(["revert", "-p", cid])
            .check_run()
            .await
            .context("Failed to revert container")?;

        let mut cmd = tokio::process::Command::new("pot");
        cmd.args(["start", "-p", cid])
            .check_run()
            .await
            .context("Failed to (re)start container")?;
        Ok(())
    }

    // Failing during cleanup is pretty unexpected, but we can still return the successful compile
    // TODO: if too many of these fail, we should mark this builder as faulty
    async fn finish_container(
        job_id: &str,
        container_lists: Arc<Mutex<HashMap<PathBuf, Vec<String>>>>,
        toolchain_dir: &Path,
        cid: &str,
        pot_cmd: &Path,
    ) {
        if let Err(e) = Self::clean_container(cid).await {
            tracing::info!("[finish_container({job_id})]: Failed to clean container {cid}: {e}");
            if let Err(e) = pot_rm(cid, pot_cmd).await {
                tracing::warn!(
                    "[finish_container({job_id})]: Failed to remove container {cid} after failed clean: {e}"
                );
            }
            return;
        }

        // Good as new, add it back to the container list
        if let Some(entry) = container_lists
            .lock()
            .await
            .get_mut(&toolchain_dir.to_path_buf())
        {
            tracing::debug!("[finish_container({job_id})]: Reclaimed container {cid}");
            entry.push(cid.to_owned())
        } else {
            tracing::warn!(
                "[finish_container({job_id})]: Was ready to reclaim container {cid} but toolchain went missing"
            );
            if let Err(e) = pot_rm(cid, pot_cmd).await {
                tracing::warn!(
                    "[finish_container({job_id})]: Failed to remove container {cid}: {e}"
                );
            }
        }
    }

    async fn make_image(
        job_id: &str,
        toolchain_dir: &Path,
        pot_fs_root: &Path,
        clone_from: &str,
        pot_cmd: &PathBuf,
        pot_clone_args: &[String],
    ) -> Result<String> {
        let toolchain_name = toolchain_dir.components().next_back().unwrap();
        let imagename = format!(
            "sccache-image-{}",
            toolchain_name.as_os_str().to_string_lossy()
        );
        tracing::trace!("[make_image({job_id})]: Creating toolchain image: {imagename}");
        let mut clone_args: Vec<&str> = ["clone", "-p", &imagename, "-P", clone_from].to_vec();
        clone_args.append(&mut pot_clone_args.iter().map(|s| s as &str).collect());
        let mut cmd = tokio::process::Command::new(pot_cmd);
        cmd.args(&clone_args)
            .check_run()
            .await
            .context("Failed to create pot container")?;

        tracing::trace!("[make_image({job_id})]: Copying in toolchain");

        let jail_root = pot_fs_root.join("jails").join(&imagename).join("m");

        // Copy inputs to jail_root
        for j_path in list_dirs(toolchain_dir)
            .iter()
            .filter_map(|h_path| h_path.strip_prefix(toolchain_dir).ok())
            .map(|c_path| jail_root.join(c_path))
            .sorted_by(|a, b| b.cmp(a))
        {
            if !j_path.exists() {
                tracing::trace!("[make_image({job_id})]: creating toolchain dir {j_path:?}");
                tokio::fs::create_dir_all(&j_path)
                    .await
                    .context(format!("Failed to create toolchain directory {j_path:?}"))?;
            }
        }

        for (h_path, j_path, c_path) in list_files(toolchain_dir).iter().filter_map(|h_path| {
            h_path.strip_prefix(toolchain_dir).ok().map(|c_path| {
                (
                    // (src) host path
                    h_path.as_path(),
                    // (dst) host path in the jail root
                    jail_root.join(c_path),
                    // (dst) container path for debugging
                    Path::new("/").join(c_path),
                )
            })
        }) {
            tracing::trace!(
                "[make_image({job_id})]: copying toolchain file {h_path:?} -> {j_path:?}"
            );
            tokio::fs::copy(h_path, j_path)
                .await
                .context(format!("Failed to copy toolchain file {c_path:?}"))?;
        }

        let mut cmd = tokio::process::Command::new(pot_cmd);
        cmd.args(["snapshot", "-p", &imagename])
            .check_run()
            .await
            .context("Failed to snapshot container after build")?;

        Ok(imagename)
    }

    async fn start_container(
        image: &str,
        pot_cmd: &PathBuf,
        pot_clone_args: &[String],
    ) -> Result<String> {
        let cid = format!("sccache-builder-{}", Uuid::new_v4());
        let mut clone_args: Vec<&str> = ["clone", "-p", &cid, "-P", image].to_vec();
        clone_args.append(&mut pot_clone_args.iter().map(|s| s as &str).collect());
        let mut cmd = tokio::process::Command::new(pot_cmd);
        cmd.args(&clone_args)
            .check_run()
            .await
            .context("Failed to create pot container")?;

        let mut cmd = tokio::process::Command::new(pot_cmd);
        cmd.args(["snapshot", "-p", &cid])
            .check_run()
            .await
            .context("Failed to snapshotpot container")?;

        let mut cmd = tokio::process::Command::new(pot_cmd);
        cmd.args(["start", "-p", &cid])
            .check_run()
            .await
            .context("Failed to start container")?;

        Ok(cid.to_string())
    }

    async fn perform_build(
        job_id: &str,
        CompileCommand {
            executable,
            arguments,
            env_vars,
            cwd,
        }: CompileCommand,
        output_paths: Vec<String>,
        inputs: opendal::Buffer,
        cid: &str,
        pot_fs_root: &Path,
        job_queue: &tokio::sync::Semaphore,
    ) -> BuildResult {
        tracing::trace!("[perform_build({job_id})]: Compile environment: {env_vars:?}");
        tracing::trace!("[perform_build({job_id})]: Compile command: {executable:?} {arguments:?}");
        tracing::trace!("[perform_build({job_id})]: Output paths: {output_paths:?}");

        if output_paths.is_empty() {
            return Err(BuildError::Unknown(anyhow!("Output paths is empty")));
        }

        // Do as much asyncio work as possible before acquiring a job slot

        {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            tracing::trace!("[perform_build({job_id})]: copying in inputs");
            let jail_root = pot_fs_root.join("jails").join(cid).join("m");
            tokio::task::spawn_blocking(move || {
                // Copy inputs to jail_root
                tar::Archive::new(flate2::read::ZlibDecoder::new(inputs.reader()))
                    .unpack(&jail_root)
                    .context("Failed to unpack inputs to tempdir")
                    .map_err(BuildError::UnpackInputs)
            })
            .await
            .map_err(anyhow::Error::new)
            .map_err(BuildError::UnpackInputs)??;
        }

        let cwd = Path::new(&cwd);

        // Canonicalize output path as either absolute or relative to cwd
        let output_paths_absolute = output_paths
            .iter()
            .map(|path| cwd.join(Path::new(path)))
            .collect::<Vec<_>>();

        {
            // Bail early if job_queue is closed while this job is running
            if job_queue.is_closed() {
                return Err(BuildError::Cancelled);
            }

            tracing::trace!("[perform_build({job_id})]: creating output directories");

            let mut cmd = tokio::process::Command::new("jexec");

            cmd.args([cid, "mkdir", "-p"]).arg(cwd);

            for path in output_paths_absolute.iter() {
                // If it doesn't have a parent, nothing needs creating
                if let Some(path) = path.parent() {
                    cmd.arg(path);
                }
            }

            tracing::trace!(
                "[perform_build({job_id})]: mkdir command: {:?}",
                cmd.as_std()
            );

            cmd.check_run()
                .await
                .context("Failed to create directories required for compiling in container")
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
            let mut cmd = tokio::process::Command::new("jexec");
            cmd.arg(cid);
            cmd.arg("env");
            for (k, v) in env_vars {
                if k.contains('=') {
                    tracing::warn!(
                        "[perform_build({job_id})]: Skipping environment variable: {k:?}"
                    );
                    continue;
                }
                let mut env = k;
                env.push('=');
                env.push_str(&v);
                cmd.arg(env);
            }
            let shell_cmd = "cd \"$1\" && shift && exec \"$@\"";
            cmd.args(["sh", "-c", shell_cmd]);
            cmd.arg(&executable);
            cmd.arg(cwd);
            cmd.arg(&executable);
            cmd.args(&arguments);

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
                tracing::trace!("[perform_build({job_id})]: retrieving {output_paths:?}");

                for (path, abs_path) in output_paths.into_iter().zip(output_paths_absolute) {
                    // TODO: this isn't great, but cp gives it out as a tar
                    let output = tokio::process::Command::new("jexec")
                        .args([cid, "cat"])
                        .arg(&abs_path)
                        .output()
                        .await;

                    match output {
                        Ok(output) => {
                            if output.status.success() {
                                match OutputData::try_from_reader(&*output.stdout) {
                                    Ok(data) => outputs.push((path, data)),
                                    Err(err) => {
                                        tracing::error!(
                                            "[perform_build({job_id})]: Failed to read and compress output file host={abs_path:?}, container={path:?}: {err}"
                                        )
                                    }
                                }
                            } else {
                                tracing::debug!(
                                    "[perform_build({job_id})]: Missing output path {path:?} ({abs_path:?})"
                                )
                            }
                        }
                        Err(err) => {
                            return Err(BuildError::ReadBuildResult(abs_path, err));
                        }
                    }
                }
            }

            outputs
        };

        BuildResult::Ok(BuildOutput { output, outputs })
    }
}

#[async_trait]
impl BuilderIncoming for PotBuilder {
    // From Server
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

        tracing::debug!("[run_build({job_id})]: Finding container");

        let cid = self
            .get_container(job_id, toolchain_dir)
            .await
            .context("Failed to get a container for build")
            .map_err(BuildError::Unknown)?;

        self.cids
            .lock()
            .await
            .insert(job_id.to_owned(), (cid.clone(), toolchain_dir.to_owned()));

        tracing::debug!("[run_build({job_id})]: Performing build in container {cid}");

        let res = Self::perform_build(
            job_id,
            command,
            outputs,
            inputs,
            &cid,
            &self.pot_fs_root,
            self.job_queue.as_ref(),
        )
        .await;

        tracing::debug!("[run_build({job_id})]: Returning result");

        res
    }

    async fn finish_build(&self, job_id: &str) {
        if let Some((cid, toolchain_dir)) = self.cids.lock().await.remove(job_id) {
            tracing::debug!("[finish_build({job_id})]: Finishing with container {cid}");
            Self::finish_container(
                job_id,
                self.container_lists.clone(),
                &toolchain_dir,
                &cid,
                &self.pot_cmd,
            )
            .await;
        }
    }

    async fn shutdown(&self) {
        self.job_queue.close();
        futures::future::join_all(self.cids.lock().await.drain().map(
            |(job_id, (cid, toolchain_dir))| {
                let pot_cmd = self.pot_cmd.clone();
                let container_lists = self.container_lists.clone();
                async move {
                    tracing::debug!("[shutdown({job_id})]: Finishing with container {cid}");
                    Self::finish_container(&job_id, container_lists, &toolchain_dir, &cid, &pot_cmd)
                        .await
                }
            },
        ))
        .await;
    }
}
