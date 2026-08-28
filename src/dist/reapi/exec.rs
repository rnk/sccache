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

//! Turning an sccache `CompileCommand` into an REv2 `Action`, and driving it
//! to completion.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use tonic::transport::Channel;

use crate::{config::ReapiToolchainMode, dist::CompileCommand, errors::*};

use super::{
    RpcContext,
    merkle::{DirBuilder, digest_message},
    paths::{relativize, root_relative, strip_root},
    proto::build::bazel::remote::execution::v2 as reapi,
};

use reapi::execution_client::ExecutionClient;

/// Everything needed to describe one remote compilation.
pub struct ActionPlan {
    pub command: reapi::Command,
    /// The `Action` and `Command` blobs, which have to be in the CAS before
    /// `Execute` is called.
    pub blobs: Vec<(reapi::Digest, Vec<u8>)>,
    pub action_digest: reapi::Digest,
    /// Remote (working-directory-relative) output path -> original dist path,
    /// so results can be mapped back to where sccache expects them.
    pub output_paths: BTreeMap<String, String>,
}

/// Should this argument be rewritten from an absolute dist path to a path
/// relative to the working directory?
///
/// Only paths we are actually shipping, or that the action is expected to
/// produce, are rewritten. An absolute path that names neither -- an
/// `-idirafter /opt/vendor/...` pointing into the worker's container image, say
/// -- is deliberately left alone.
/// Does `path` -- the input-root-relative form of `arg` -- name something the
/// action either ships or produces?
fn should_rewrite(path: &str, tree: &DirBuilder, outputs: &[String]) -> bool {
    outputs.iter().any(|o| root_relative(o) == path) || tree.contains(path)
}

fn rewrite_arg(
    arg: &str,
    working_directory: &str,
    tree: &DirBuilder,
    outputs: &[String],
) -> String {
    // A bare path: the compile input, the `-o` value, an `-I` given as two
    // separate arguments.
    if arg.starts_with('/') {
        let path = root_relative(arg);
        if should_rewrite(&path, tree, outputs) {
            return relativize(&path, working_directory);
        }
    }

    // A path glued to a flag: `-I/abs/path`, `--sysroot=/`, `-L/abs/path`,
    // `-isystem/abs/path`. Splitting at the first `/` handles every spelling
    // without needing a table of which flags take paths -- and `-I/path` is
    // the spelling that actually appears in compile databases.
    //
    // This is safe precisely because `should_rewrite` gates it: an argument
    // like `-DFOO=/not/a/path` splits into `-DFOO=` and `/not/a/path`, finds
    // nothing by that name in the input root, and is left alone.
    if arg.starts_with('-')
        && let Some(index) = arg.find('/')
    {
        let (flag, value) = arg.split_at(index);
        let path = root_relative(value);
        if should_rewrite(&path, tree, outputs) {
            return format!("{flag}{}", relativize(&path, working_directory));
        }
    }

    arg.to_owned()
}

/// Build the `Action`/`Command` pair for one compilation.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    command: &CompileCommand,
    outputs: &[String],
    input_root: &reapi::Digest,
    tree: &DirBuilder,
    toolchain_mode: ReapiToolchainMode,
    platform: &BTreeMap<String, String>,
    env_passthrough: &[String],
    timeout: Duration,
    do_not_cache: bool,
) -> Result<ActionPlan> {
    let working_directory = root_relative(&command.cwd);

    let executable = match toolchain_mode {
        ReapiToolchainMode::Image => command.executable.clone(),
        ReapiToolchainMode::Inputs => {
            let in_tree = strip_root(&command.executable);
            if !tree.contains(in_tree) {
                bail!(
                    "Toolchain mode is \"inputs\" but the compiler {:?} is not in the action's \
                     input root. Either package the toolchain, or set \
                     `[dist.reapi] toolchain = \"image\"` if the compiler is baked into the \
                     worker's container image.",
                    command.executable
                );
            }
            relativize(in_tree, &working_directory)
        }
    };

    let mut arguments = Vec::with_capacity(command.arguments.len() + 1);
    arguments.push(executable);
    arguments.extend(
        command
            .arguments
            .iter()
            .map(|arg| rewrite_arg(arg, &working_directory, tree, outputs)),
    );

    // Map each output back to where sccache wants it written locally.
    let output_paths: BTreeMap<String, String> = outputs
        .iter()
        .map(|out| (relativize(strip_root(out), &working_directory), out.clone()))
        .collect();

    // Only a curated set of environment variables is forwarded. Shipping the
    // client's whole environment would leak secrets to the worker and would
    // give every developer a different action digest for identical work,
    // making the server's action cache useless.
    let environment_variables: Vec<reapi::command::EnvironmentVariable> = command
        .env_vars
        .iter()
        .filter(|(name, _)| env_passthrough.iter().any(|allowed| allowed == name))
        .map(|(name, value)| (name.clone(), value.clone()))
        // REv2 requires environment variables in name order.
        .collect::<BTreeMap<String, String>>()
        .into_iter()
        .map(|(name, value)| reapi::command::EnvironmentVariable { name, value })
        .collect();

    let platform = reapi::Platform {
        properties: platform
            .iter()
            .map(|(name, value)| reapi::platform::Property {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
    };

    // `output_files` and `output_directories` are the pre-2.1 spelling of
    // `output_paths` and MUST be left empty: the spec says that when
    // `output_paths` is used, these are ignored. `Command.platform` is the
    // pre-2.2 spelling of `Action.platform`; both are populated, as Bazel
    // does, so that older servers still see the platform.
    #[allow(deprecated)]
    let command_msg = reapi::Command {
        arguments,
        environment_variables,
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_paths: output_paths.keys().cloned().collect(),
        platform: Some(platform.clone()),
        working_directory,
        output_node_properties: Vec::new(),
        // We never request output directories, only files.
        output_directory_format: reapi::command::OutputDirectoryFormat::TreeOnly as i32,
    };

    let (command_digest, command_bytes) = digest_message(&command_msg);

    let action = reapi::Action {
        command_digest: Some(command_digest.clone()),
        input_root_digest: Some(input_root.clone()),
        timeout: Some(prost_types::Duration {
            seconds: timeout.as_secs() as i64,
            nanos: 0,
        }),
        do_not_cache,
        salt: Vec::new(),
        platform: Some(platform),
    };

    let (action_digest, action_bytes) = digest_message(&action);

    Ok(ActionPlan {
        blobs: vec![
            (command_digest, command_bytes),
            (action_digest.clone(), action_bytes),
        ],
        command: command_msg,
        action_digest,
        output_paths,
    })
}

/// Run an action to completion, following the `Operation` stream and
/// reconnecting with `WaitExecution` if the stream drops early.
pub async fn execute(
    rpc: &Arc<RpcContext>,
    channel: &Channel,
    action_digest: &reapi::Digest,
    skip_cache_lookup: bool,
) -> Result<reapi::ExecuteResponse> {
    use futures::StreamExt;

    let mut client = ExecutionClient::new(channel.clone())
        .max_decoding_message_size(usize::MAX)
        .max_encoding_message_size(usize::MAX);

    let mut stream = client
        .execute(rpc.request(reapi::ExecuteRequest {
            instance_name: rpc.instance_name.clone(),
            skip_cache_lookup,
            action_digest: Some(action_digest.clone()),
            execution_policy: None,
            results_cache_policy: None,
            digest_function: reapi::digest_function::Value::Sha256 as i32,
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
        }))
        .await
        .map_err(|e| super::rpc_error(e, "Execute"))?
        .into_inner();

    let mut operation_name = String::new();
    let mut reconnected = false;

    loop {
        match stream.next().await {
            Some(Ok(operation)) => {
                if !operation.name.is_empty() {
                    operation_name = operation.name.clone();
                }
                log_stage(&operation, action_digest);
                if operation.done {
                    return finish(operation);
                }
            }
            Some(Err(status)) => return Err(super::rpc_error(status, "Execute")),
            // The server closed the stream without ever reporting the
            // operation done. This happens routinely with load balancers and
            // idle timeouts on long compiles, and is exactly what
            // WaitExecution exists for.
            None => {
                if reconnected || operation_name.is_empty() {
                    bail!(
                        "Execution stream for action {} ended before the operation completed",
                        action_digest.hash
                    );
                }
                debug!(
                    "Execution stream for {} ended early, reattaching with WaitExecution",
                    action_digest.hash
                );
                reconnected = true;
                stream = client
                    .wait_execution(rpc.request(reapi::WaitExecutionRequest {
                        name: operation_name.clone(),
                    }))
                    .await
                    .map_err(|e| super::rpc_error(e, "WaitExecution"))?
                    .into_inner();
            }
        }
    }
}

fn log_stage(
    operation: &super::proto::google::longrunning::Operation,
    action_digest: &reapi::Digest,
) {
    use prost::Message;

    let Some(metadata) = operation.metadata.as_ref() else {
        return;
    };
    let Ok(metadata) = reapi::ExecuteOperationMetadata::decode(&metadata.value[..]) else {
        return;
    };
    let stage = reapi::execution_stage::Value::try_from(metadata.stage)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| metadata.stage.to_string());
    trace!("[{}]: remote execution stage {stage}", action_digest.hash);
}

fn finish(
    operation: super::proto::google::longrunning::Operation,
) -> Result<reapi::ExecuteResponse> {
    use super::proto::google::longrunning::operation::Result as OpResult;
    use prost::Message;

    match operation.result {
        Some(OpResult::Response(any)) => reapi::ExecuteResponse::decode(&any.value[..])
            .context("Failed to decode ExecuteResponse from completed operation"),
        Some(OpResult::Error(status)) => Err(super::status_error(&status, "Execute")),
        None => bail!("Operation {} completed with no result", operation.name),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn tree_with(paths: &[&str]) -> DirBuilder {
        let mut tree = DirBuilder::default();
        for path in paths {
            tree.insert_file(path, super::super::merkle::digest_bytes(b""), false)
                .unwrap();
        }
        tree
    }

    #[test]
    fn only_known_paths_are_rewritten() {
        let wd = "work/proj/build";
        let tree = tree_with(&["work/proj/src/a.dist.cpp"]);
        let outputs = vec!["/work/proj/build/a.o".to_owned()];

        // The compile input is in the tree, so it is rewritten.
        assert_eq!(
            rewrite_arg("/work/proj/src/a.dist.cpp", wd, &tree, &outputs),
            "../src/a.dist.cpp"
        );
        // The output does not exist yet, but we know it is one.
        assert_eq!(
            rewrite_arg("/work/proj/build/a.o", wd, &tree, &outputs),
            "a.o"
        );
        // An absolute path into the worker's image is left alone.
        assert_eq!(
            rewrite_arg("/opt/vendor/include", wd, &tree, &outputs),
            "/opt/vendor/include"
        );
        // Ordinary flags are untouched.
        assert_eq!(rewrite_arg("-O3", wd, &tree, &outputs), "-O3");
        assert_eq!(
            rewrite_arg("-DFOO=/not/a/real/path", wd, &tree, &outputs),
            "-DFOO=/not/a/real/path"
        );
    }

    #[test]
    fn glued_flag_values_are_rewritten() {
        let wd = "work/proj/build";
        let tree = tree_with(&["work/proj/sysroot/x", "work/proj/include/a.h"]);
        assert_eq!(
            rewrite_arg("--sysroot=/work/proj/sysroot/x", wd, &tree, &[]),
            "--sysroot=../sysroot/x"
        );
        // The spelling that actually shows up in every compile database.
        assert_eq!(
            rewrite_arg("-I/work/proj/include", wd, &tree, &[]),
            "-I../include"
        );
        assert_eq!(
            rewrite_arg("-isystem/work/proj/include", wd, &tree, &[]),
            "-isystem../include"
        );
        // `--sysroot=/` names the input root itself.
        assert_eq!(
            rewrite_arg("--sysroot=/", wd, &tree, &[]),
            "--sysroot=../../.."
        );
        // An include path we are not shipping stays absolute, and a macro that
        // merely looks like a path is untouched.
        assert_eq!(
            rewrite_arg("-I/opt/elsewhere", wd, &tree, &[]),
            "-I/opt/elsewhere"
        );
        assert_eq!(
            rewrite_arg("-DFOO=/not/a/path", wd, &tree, &[]),
            "-DFOO=/not/a/path"
        );
    }

    fn test_command() -> CompileCommand {
        CompileCommand {
            executable: "/work/toolchain/bin/clang++".to_owned(),
            arguments: vec![
                "-x".to_owned(),
                "c++-cpp-output".to_owned(),
                "-O3".to_owned(),
                "-c".to_owned(),
                "/work/proj/src/a.dist.cpp".to_owned(),
                "-o".to_owned(),
                "/work/proj/build/a.o".to_owned(),
            ],
            env_vars: vec![
                ("SOURCE_DATE_EPOCH".to_owned(), "0".to_owned()),
                ("AWS_SECRET_ACCESS_KEY".to_owned(), "hunter2".to_owned()),
            ],
            cwd: "/work/proj/build".to_owned(),
        }
    }

    fn test_plan(mode: ReapiToolchainMode) -> ActionPlan {
        let tree = tree_with(&["work/proj/src/a.dist.cpp", "work/toolchain/bin/clang++"]);
        let (root, _) = tree.finish();
        plan(
            &test_command(),
            &["/work/proj/build/a.o".to_owned()],
            &root,
            &tree,
            mode,
            &BTreeMap::from([("OSFamily".to_owned(), "Linux".to_owned())]),
            &["SOURCE_DATE_EPOCH".to_owned()],
            Duration::from_secs(600),
            false,
        )
        .unwrap()
    }

    #[test]
    fn inputs_mode_uses_a_relative_compiler() {
        let command = test_plan(ReapiToolchainMode::Inputs).command;

        assert_eq!(command.working_directory, "work/proj/build");
        assert_eq!(command.arguments[0], "../../toolchain/bin/clang++");
        assert_eq!(command.arguments[5], "../src/a.dist.cpp");
        assert_eq!(command.arguments[7], "a.o");
        assert_eq!(command.output_paths, vec!["a.o".to_owned()]);
    }

    #[test]
    fn image_mode_keeps_the_absolute_compiler() {
        let plan = test_plan(ReapiToolchainMode::Image);
        assert_eq!(plan.command.arguments[0], "/work/toolchain/bin/clang++");
    }

    #[test]
    fn only_allowlisted_env_vars_are_sent() {
        let command = test_plan(ReapiToolchainMode::Inputs).command;

        let names: Vec<&str> = command
            .environment_variables
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert_eq!(names, vec!["SOURCE_DATE_EPOCH"]);
    }

    #[test]
    fn the_action_digest_is_stable() {
        // The whole point of the Merkle tree is that identical work hashes
        // identically. If this ever changes, remote action cache hit rates
        // silently drop to zero, so pin it.
        let a = test_plan(ReapiToolchainMode::Inputs);
        let b = test_plan(ReapiToolchainMode::Inputs);
        assert_eq!(a.action_digest, b.action_digest);
        assert_eq!(a.action_digest.hash.len(), 64);
    }
}
