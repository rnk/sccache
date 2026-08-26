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
    mock_command::CommandCreatorSync,
    server::SccacheService,
};

use async_trait::async_trait;

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use crate::errors::*;

/// A unit struct on which to implement `CCompilerImpl`.
#[derive(Clone, Debug)]
pub struct Ptxas {
    pub version: Option<String>,
}

#[async_trait]
impl CCompilerImpl for Ptxas {
    fn kind(&self) -> CCompilerKind {
        CCompilerKind::Ptxas
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
        cicc::parse_arguments(arguments, cwd, Language::Cubin, &ARGS[..])
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
            "-o",
        )
    }
}

use cicc::ArgData::*;

counted_array!(pub static ARGS: [ArgInfo<cicc::ArgData>; _] = [
    flag!("--compile-only", PassThroughFlag),
    flag!("--disable-warnings", PassThroughFlag),
    flag!("--dont-merge-basicblocks", PassThroughFlag),
    flag!("--return-at-end", PassThroughFlag),
    take_arg!("--split-compile", OsString, CanBeSeparated, Unhashed),
    take_arg!("--split-compile=", OsString, Concatenated, Unhashed),
    take_arg!("-arch", OsString, CanBeSeparated(b'='), PassThrough),
    flag!("-g", PassThroughFlag),
    take_arg!("-m", OsString, Concatenated, PassThrough),
    take_arg!("-o", PathBuf, Separated, Output),
    take_arg!("-split-compile", OsString, CanBeSeparated, Unhashed),
    take_arg!("-split-compile=", OsString, Concatenated, Unhashed),
]);
