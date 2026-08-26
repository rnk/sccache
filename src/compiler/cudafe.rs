// Copyright 2016 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright (c) 2024 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
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
    compiler::{
        c::{CCompilerImpl, CCompilerKind, DepfilePath, ParsedArguments, PreprocessorOutput},
        cicc, {Cacheable, CompilerArguments, Language},
        {CompileCommandImpl, args::*},
    },
    counted_array, dist,
    errors::*,
    mock_command::CommandCreatorSync,
    server::SccacheService,
};

use async_trait::async_trait;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// A unit struct on which to implement `CCompilerImpl`.
#[derive(Clone, Debug)]
pub struct CudaFE {
    pub version: Option<String>,
}

#[async_trait]
impl CCompilerImpl for CudaFE {
    fn kind(&self) -> CCompilerKind {
        CCompilerKind::CudaFE
    }
    fn plusplus(&self) -> bool {
        true
    }
    fn version(&self) -> Option<String> {
        self.version.clone()
    }
    fn parse_arguments(
        &self,
        arguments: &[OsString],
        cwd: &Path,
        _env_vars: &[(OsString, OsString)],
    ) -> CompilerArguments<ParsedArguments> {
        cicc::parse_arguments(arguments, cwd, Language::CudaFE, &ARGS[..])
    }
    #[allow(clippy::too_many_arguments)]
    async fn preprocess<T>(
        &self,
        _service: &SccacheService<T>,
        _creator: &T,
        _executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        _env_vars: &[(OsString, OsString)],
        _rewrite_includes_only: bool,
        _generate_dependencies: bool,
        _include_line_numbers: bool,
    ) -> Result<PreprocessorOutput>
    where
        T: CommandCreatorSync,
    {
        cicc::preprocess(cwd, parsed_args).await
    }
    async fn generate_dependencies<T>(
        &self,
        _creator: &T,
        _executable: &Path,
        _parsed_args: &ParsedArguments,
        _cwd: &Path,
        _env_vars: &[(OsString, OsString)],
    ) -> Result<Option<DepfilePath>>
    where
        T: CommandCreatorSync,
    {
        Ok(None)
    }
    fn generate_compile_commands(
        &self,
        path_transformer: &mut dist::PathTransformer,
        executable: &Path,
        parsed_args: &ParsedArguments,
        cwd: &Path,
        env_vars: &[(OsString, OsString)],
        _rewrite_includes_only: bool,
        _stage_sources: bool,
        _hash_key: &str,
    ) -> Result<(
        impl CompileCommandImpl,
        Option<dist::CompileCommand>,
        Cacheable,
    )> {
        cicc::generate_compile_commands(
            path_transformer,
            executable,
            parsed_args,
            cwd,
            env_vars,
            "--module_id_file_name",
        )
    }
}

use cicc::ArgData::*;

counted_array!(pub static ARGS: [ArgInfo<cicc::ArgData>; _] = [
    flag!("--allow_managed", PassThroughFlag),
    take_arg!("--c++", OsString, Concatenated, PassThrough),
    flag!("--device-hidden-visibility", PassThroughFlag),
    flag!("--display_error_number", PassThroughFlag),
    flag!("--enable-tile", PassThroughFlag),
    take_arg!("--gen_c_file_name", PathBuf, Separated, ExtraOutput),
    flag!("--gen_module_id_file", GenModuleIdFileFlag),
    take_arg!("--gnu_version", OsString, Concatenated(b'='), PassThrough),
    take_arg!("--m", OsString, Concatenated, PassThrough),
    take_arg!("--module_id_file_name", PathBuf, Separated, Output),
    take_arg!("--orig_src_file_name", OsString, Separated, PassThrough),
    take_arg!("--orig_src_path_name", OsString, Separated, PassThrough),
    flag!("--parse_templates", PassThroughFlag),
    flag!("--static-host-stub", PassThroughFlag),
    take_arg!("--stub_file_name", OsString, Separated, PassThrough),
]);
