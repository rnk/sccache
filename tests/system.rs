// System tests for compiling C code.
//
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

#![deny(rust_2018_idioms)]

#[macro_use]
extern crate log;

mod harness;

use assert_cmd::prelude::*;
use fs::File;
use fs_err as fs;
use harness::{
    Compiler,
    client::{SccacheClient, make_sccache_client},
    compile_cmdline, find_compilers, write_source,
};
use pastey::paste;
use predicates::prelude::*;
use regex::Regex;
#[cfg(unix)]
use serial_test::serial;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs as unix_fs;
use std::path::Path;
use std::process::Command;
use std::str;
use std::time::{Duration, SystemTime};
use test_case::test_case;

fn adv_key_kind(lang: &str, compiler: &str) -> String {
    let language = lang.to_owned();
    match compiler {
        "clang" | "clang++" | "clang-cl" => language + " [clang]",
        "gcc" | "g++" => language + " [gcc]",
        "cl" => language + " [msvc]",
        "nvc" | "nvc++" => language + " [nvhpc]",
        "nvcc" => match lang {
            "cudafe++" => "cuda [cudafe++]".to_owned(),
            "ptx" => language + " [cicc]",
            "cubin" => language + " [ptxas]",
            _ => language + " [nvcc]",
        },
        _ => {
            trace!("Unknown compiler type: {compiler}");
            language + "unknown"
        }
    }
}

//TODO: could test gcc when targeting mingw.

macro_rules! vec_from {
    ( $t:ty, $( $x:expr ),* ) => {
        vec!($( Into::<$t>::into(&$x), )*)
    };
}

// TODO: This will fail if gcc/clang is actually a ccache wrapper, as it is the
// default case on Fedora, e.g.
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn compile_cuda_cmdline<T: AsRef<OsStr>>(
    compiler: &str,
    exe: T,
    compile_flag: &str,
    input: &str,
    output: &str,
    extra_args: &[OsString],
) -> Vec<OsString> {
    let mut args = match compiler {
        "nvcc" => vec_from!(
            OsString,
            exe.as_ref(),
            "-allow-unsupported-compiler",
            "-x",
            "cu",
            compile_flag,
            input,
            "-o",
            output
        ),
        "clang++" => {
            vec_from!(
                OsString,
                exe,
                "-x",
                "cuda",
                compile_flag,
                input,
                "--cuda-gpu-arch=sm_75",
                format!(
                    "--cuda-path={}",
                    env::var_os("CUDA_PATH")
                        .or(env::var_os("CUDA_HOME"))
                        .unwrap_or("/usr/local/cuda".into())
                        .to_string_lossy()
                ),
                "--no-cuda-version-check",
                // work around for clang-cuda on windows-2019 (https://github.com/microsoft/STL/issues/2359)
                "-D_ALLOW_COMPILER_AND_STL_VERSION_MISMATCH",
                "-o",
                output
            )
        }
        _ => panic!("Unsupported compiler: {compiler}"),
    };
    if !extra_args.is_empty() {
        args.append(&mut extra_args.to_vec());
    }
    args.iter()
        .filter(|x| !x.is_empty())
        .cloned()
        .collect::<Vec<_>>()
}

// TODO: This will fail if gcc/clang is actually a ccache wrapper, as it is the
// default case on Fedora, e.g.
//
// archs is a list of GPU architectures to compile for.
fn compile_hip_cmdline<T: AsRef<OsStr>>(
    compiler: &str,
    exe: T,
    input: &str,
    output: &str,
    archs: &Vec<String>,
    extra_args: impl IntoIterator<Item = OsString>,
) -> Vec<OsString> {
    let mut arg = match compiler {
        "clang" => {
            vec_from!(OsString, exe, "-x", "hip", "-c", input, "-o", output)
        }
        _ => panic!("Unsupported compiler: \"{compiler}\""),
    };
    for arch in archs {
        arg.push(format!("--offload-arch={arch}").into());
    }
    arg.extend(extra_args);
    arg
}

const INPUT: &str = "test.c";
const INPUT_CLANG_MULTICALL: &str = "test_clang_multicall.c";
const INPUT_WITH_WHITESPACE: &str = "test_whitespace.c";
const INPUT_WITH_WHITESPACE_ALT: &str = "test_whitespace_alt.c";
const INPUT_ERR: &str = "test_err.c";
const INPUT_MACRO_EXPANSION: &str = "test_macro_expansion.c";
const INPUT_WITH_DEFINE: &str = "test_with_define.c";
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const INPUT_FOR_CUDA_A: &str = "test_a.cu";
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const INPUT_FOR_CUDA_A_COPY: &str = "test_a_copy.cu";
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const INPUT_FOR_CUDA_A_INTERNAL_LINKAGE: &str = "test_a_internal_linkage.cu";
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const INPUT_FOR_CUDA_B: &str = "test_b.cu";
#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
const INPUT_FOR_CUDA_C: &str = "test_c.cu";
const INPUT_FOR_HIP_A: &str = "test_a.hip";
const INPUT_FOR_HIP_B: &str = "test_b.hip";
const OUTPUT: &str = "test.o";

// Copy the source files into the tempdir so we can compile with relative paths, since the commandline winds up in the hash key.
fn copy_to_tempdir(inputs: &[&str], tempdir: &Path) {
    for f in inputs {
        let original_source_file = Path::new(file!()).parent().unwrap().join(f);
        let source_file = tempdir.join(f);
        trace!("fs::copy({original_source_file:?}, {source_file:?})");
        fs::copy(&original_source_file, &source_file).unwrap();
        // Preprocessor cache will not cache files that are too recent.
        // Certain OS/FS combinations have a slow resolution (up to 2s for NFS),
        // leading to flaky tests.
        // We set the times for the new file to 10 seconds ago, to be safe.
        let new_time =
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(10));
        filetime::set_file_times(source_file, new_time, new_time).unwrap();
    }
}

fn test_basic_compile(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_basic_compile: {name}");
    // Compile a source file.
    copy_to_tempdir(&[INPUT, INPUT_ERR], tempdir);

    let out_file = tempdir.join(OUTPUT);
    trace!("compile");
    client
        .cmd()
        .args(compile_cmdline(name, exe, INPUT, OUTPUT, []))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.compile_requests);
    assert_eq!(1, stats.requests_executed);
    assert_eq!(0, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    if preprocessor_cache_mode {
        assert_eq!(1, stats.preprocessor_cache_misses.all());
    }
    assert_eq!(&1, stats.cache_misses.get("C/C++").unwrap());
    let adv_key = adv_key_kind("c", compiler.name);
    assert_eq!(&1, stats.cache_misses.get_adv(&adv_key).unwrap());
    trace!("compile");
    fs::remove_file(&out_file).unwrap();
    client
        .cmd()
        .args(compile_cmdline(name, exe, INPUT, OUTPUT, []))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(2, stats.compile_requests);
    assert_eq!(2, stats.requests_executed);
    assert_eq!(1, stats.cache_hits.all());
    if preprocessor_cache_mode {
        assert_eq!(1, stats.preprocessor_cache_hits.all());
    }
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("C/C++").unwrap());
    assert_eq!(&1, stats.cache_misses.get("C/C++").unwrap());
    let adv_key = adv_key_kind("c", compiler.name);
    assert_eq!(&1, stats.cache_hits.get_adv(&adv_key).unwrap());
    assert_eq!(&1, stats.cache_misses.get_adv(&adv_key).unwrap());
}

fn test_noncacheable_stats(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;

    println!("test_noncacheable_stats: {name}");
    copy_to_tempdir(&[INPUT], tempdir);

    trace!("compile");

    client.zero_stats();

    client
        .cmd()
        .arg(exe)
        .arg("-E")
        .arg(INPUT)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.compile_requests);
    assert_eq!(0, stats.requests_executed);
    assert_eq!(1, stats.not_cached.len());
    assert_eq!(Some(&1), stats.not_cached.get("-E"));
}

fn test_msvc_deps(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    // Check that -deps works.
    trace!("compile with /sourceDependencies");
    let mut args = compile_cmdline(name, exe, INPUT, OUTPUT, []);
    args.push("/sourceDependenciestest.o.json".into());
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    // Check the contents
    let f = File::open(tempdir.join("test.o.json")).expect("Failed to open dep file");
    // MSVC deps files are JSON, which we can validate properties of, but will be
    // subtly different on different systems (Windows SDK version, for example)
    let deps: serde_json::Value = serde_json::from_reader(f).expect("Failed to read dep file");
    let source = deps["Data"]["Source"].as_str().expect("No source found");
    let source = Path::new(source).file_name().expect("No source file name");
    assert_eq!(source, INPUT);

    let includes = deps["Data"]["Includes"]
        .as_array()
        .expect("No includes found");
    assert_ne!(includes.len(), 0);
}

fn test_msvc_responsefile(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name: _,
        exe,
        env_vars,
        ..
    } = compiler;

    let out_file = tempdir.join(OUTPUT);
    let cmd_file_name = "test_msvc.rsp";
    {
        let mut file = File::create(tempdir.join(cmd_file_name)).unwrap();
        let content = format!("-c {INPUT} -Fo{OUTPUT}");
        file.write_all(content.as_bytes()).unwrap();
    }

    let args = vec_from!(OsString, exe, &format!("@{cmd_file_name}"));
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();

    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();
}

fn test_gcc_mp_werror(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    trace!("test -MP with -Werror");
    let mut args = compile_cmdline(name, exe, INPUT_ERR, OUTPUT, []);
    args.extend(vec_from!(
        OsString, "-MD", "-MP", "-MF", "foo.pp", "-Werror"
    ));
    // This should fail, but the error should be from the #error!
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .failure()
        .stderr(
            predicates::str::contains("to generate dependencies you must specify either -M or -MM")
                .from_utf8()
                .not(),
        );
}

fn test_gcc_fprofile_generate_source_changes(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    trace!("test -fprofile-generate with different source inputs");
    client.zero_stats();
    const SRC: &str = "source.c";
    write_source(
        tempdir,
        SRC,
        "/*line 1*/
#ifndef UNDEFINED
/*unused line 1*/
#endif

int main(int argc, char** argv) {
  return 0;
}
",
    );
    let mut args = compile_cmdline(name, exe, SRC, OUTPUT, []);
    args.extend(vec_from!(OsString, "-fprofile-generate"));
    trace!("compile source.c (1)");
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let stats = client.stats().unwrap();
    assert_eq!(0, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_misses.get("C/C++").unwrap());
    // Compile the same source again to ensure we can get a cache hit.
    trace!("compile source.c (2)");
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("C/C++").unwrap());
    assert_eq!(&1, stats.cache_misses.get("C/C++").unwrap());
    // Now write out a slightly different source file that will preprocess to the same thing,
    // modulo line numbers. This should not be a cache hit because line numbers are important
    // with -fprofile-generate.
    write_source(
        tempdir,
        SRC,
        "/*line 1*/
#ifndef UNDEFINED
/*unused line 1*/
/*unused line 2*/
#endif

int main(int argc, char** argv) {
  return 0;
}
",
    );
    trace!("compile source.c (3)");
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(2, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("C/C++").unwrap());
    assert_eq!(&2, stats.cache_misses.get("C/C++").unwrap());
}

/* test case like this:
    echo "int test(){}" > test.cc
    mkdir o1 o2
    sccache g++ -c -g -gsplit-dwarf test.cc -o test1.o
    sccache g++ -c -g -gsplit-dwarf test.cc -o test1.o   --- > cache hit
    sccache g++ -c -g -gsplit-dwarf test.cc -o test2.o   --- > cache miss
    strings test2.o |grep test2.dwo
*/
fn test_split_dwarf_object_generate_output_dir_changes(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    trace!("test -g -gsplit-dwarf with different output");
    client.zero_stats();
    const SRC: &str = "source.c";
    write_source(tempdir, SRC, "int test(){}");
    let mut args = compile_cmdline(name, exe.clone(), SRC, "test1.o", []);
    args.extend(vec_from!(OsString, "-g"));
    args.extend(vec_from!(OsString, "-gsplit-dwarf"));
    trace!("compile source.c (1)");
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let stats = client.stats().unwrap();
    assert_eq!(0, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_misses.get("C/C++").unwrap());
    // Compile the same source again to ensure we can get a cache hit.
    trace!("compile source.c (2)");
    client
        .cmd()
        .args(&args)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("C/C++").unwrap());
    assert_eq!(&1, stats.cache_misses.get("C/C++").unwrap());
    // Compile the same source again with different output
    // to ensure we can force generate new object file.
    let mut args2 = compile_cmdline(name, exe, SRC, "test2.o", []);
    args2.extend(vec_from!(OsString, "-g"));
    args2.extend(vec_from!(OsString, "-gsplit-dwarf"));
    trace!("compile source.c (2)");
    client
        .cmd()
        .args(&args2)
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(2, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("C/C++").unwrap());
    assert_eq!(&2, stats.cache_misses.get("C/C++").unwrap());
}

fn test_gcc_clang_no_warnings_from_macro_expansion(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_gcc_clang_no_warnings_from_macro_expansion: {name}");
    // Compile a source file.
    copy_to_tempdir(&[INPUT_MACRO_EXPANSION], tempdir);

    trace!("compile");
    client
        .cmd()
        .args(
            [
                &compile_cmdline(name, exe, INPUT_MACRO_EXPANSION, OUTPUT, [])[..],
                &vec_from!(OsString, "-Wunreachable-code")[..],
            ]
            .concat(),
        )
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success()
        .stderr(predicates::str::contains("warning:").from_utf8().not());
}

fn test_compile_with_define(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_compile_with_define: {name}");
    // Compile a source file.
    copy_to_tempdir(&[INPUT_WITH_DEFINE], tempdir);

    trace!("compile");
    client
        .cmd()
        .args(
            [
                &compile_cmdline(name, exe, INPUT_WITH_DEFINE, OUTPUT, [])[..],
                &vec_from!(OsString, "-DSCCACHE_TEST_DEFINE")[..],
            ]
            .concat(),
        )
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success()
        .stderr(predicates::str::contains("warning:").from_utf8().not());
}

fn test_gcc_clang_depfile(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_gcc_clang_depfile: {name}");
    copy_to_tempdir(&[INPUT], tempdir);
    fs::copy(tempdir.join(INPUT), tempdir.join("same-content.c")).unwrap();

    trace!("compile");
    client
        .cmd()
        .args(compile_cmdline(name, exe.clone(), INPUT, OUTPUT, []))
        .args(vec_from!(OsString, "-MD", "-MF", "first.d"))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            "same-content.c",
            "same-content.o",
            [],
        ))
        .args(vec_from!(OsString, "-MD", "-MF", "second.d"))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    let mut first = String::new();
    let mut second = String::new();
    File::open(tempdir.join("first.d"))
        .unwrap()
        .read_to_string(&mut first)
        .unwrap();
    File::open(tempdir.join("second.d"))
        .unwrap()
        .read_to_string(&mut second)
        .unwrap();
    assert_ne!(first, second);
}

fn run_sccache_command_tests(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
) {
    if compiler.name != "clang++" {
        test_basic_compile(client, compiler, tempdir, preprocessor_cache_mode);
    }
    test_compile_with_define(client, compiler, tempdir);
    if compiler.name == "cl" {
        test_msvc_deps(client, compiler, tempdir);
        test_msvc_responsefile(client, compiler, tempdir);
    }
    if compiler.name == "gcc" {
        test_gcc_mp_werror(client, compiler, tempdir);
        test_gcc_fprofile_generate_source_changes(client, compiler, tempdir);
    }
    if compiler.name == "clang" || compiler.name == "gcc" {
        test_gcc_clang_no_warnings_from_macro_expansion(client, compiler, tempdir);
        test_split_dwarf_object_generate_output_dir_changes(client, compiler, tempdir);
        test_gcc_clang_depfile(client, compiler, tempdir);
    }
    if compiler.name == "clang++" {
        test_clang_multicall(client, compiler, tempdir);
    }

    // If we are testing with clang-14 or later, we expect the -fminimize-whitespace flag to be used.
    if compiler.name == "clang" || compiler.name == "clang++" {
        let version_cmd = Command::new(compiler.exe.clone())
            .arg("--version")
            .output()
            .expect("Failure when getting compiler version");
        assert!(version_cmd.status.success());

        let version_output = match str::from_utf8(&version_cmd.stdout) {
            Ok(v) => v,
            Err(e) => panic!("Invalid UTF-8 sequence: {e}"),
        };

        // Regex to match "Apple LLVM clang version" or "Apple clang version"
        let re = Regex::new(r"(?P<apple>Apple)?.*clang version (?P<major>\d+)").unwrap();
        let (major, is_appleclang) = match re.captures(version_output) {
            Some(c) => (
                c.name("major").unwrap().as_str().parse::<usize>().unwrap(),
                c.name("apple").is_some(),
            ),
            None => panic!("Version info not found in --version output: {version_output}"),
        };
        test_clang_cache_whitespace_normalization(
            client,
            compiler,
            tempdir,
            !is_appleclang && major >= 14,
        );
    } else {
        test_clang_cache_whitespace_normalization(client, compiler, tempdir, false);
    }

    if compiler.name != "cl" {
        test_noncacheable_stats(client, compiler, tempdir);
    }
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn test_nvcc_cuda_compiles(
    client: &SccacheClient,
    compiler: &Compiler,
    host_compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
    with_debug_flags: bool,
    with_rdc: bool,
) {
    use harness::AdditionalStats;
    use sccache::compiler::{CCompilerKind, Language};

    let mut extra_args = vec![format!("-ccbin={}", host_compiler.exe.to_string_lossy()).into()];

    if with_debug_flags {
        extra_args.push("-G".into());
    }

    if with_rdc {
        extra_args.push("-rdc=true".into());
    }

    extra_args.push("-t=10".into());

    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_nvcc_cuda_compiles: {name}/{}", host_compiler.name);
    // Compile multiple source files.
    copy_to_tempdir(
        &[
            INPUT_FOR_CUDA_A,
            INPUT_FOR_CUDA_A_COPY,
            INPUT_FOR_CUDA_A_INTERNAL_LINKAGE,
            INPUT_FOR_CUDA_B,
        ],
        tempdir,
    );

    let build_dir = Path::new("build");
    fs::create_dir_all(tempdir.join(build_dir)).unwrap();

    let mut expected_stats = client.stats().unwrap();

    let mut run_cuda_test = |compile_flag: &str,
                             input: &Path,
                             output: &Path,
                             extra_args: &[OsString],
                             additional_stats: AdditionalStats| {
        client
            .cmd()
            .args(compile_cuda_cmdline(
                name,
                exe,
                compile_flag,
                input.to_string_lossy().as_ref(),
                output.to_string_lossy().as_ref(),
                extra_args,
            ))
            .current_dir(tempdir)
            .envs(env_vars.clone())
            .assert()
            .success();

        assert!(
            fs::metadata(tempdir.join(output))
                .map(|m| m.len() > 0)
                .unwrap()
        );

        fs::remove_file(tempdir.join(output)).unwrap();

        // Zero-out duration stats
        let actual_stats = client.stats().unwrap() + AdditionalStats::default();
        expected_stats += additional_stats;
        assert_eq!(expected_stats, actual_stats);
    };

    trace!("compile A ptx");
    run_cuda_test(
        "-ptx",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &build_dir.join("test.ptx"), // relative path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2),
            compilations: Some(2),
            compile_requests: Some(1),
            requests_executed: Some(2),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile A cubin");
    run_cuda_test(
        "-cubin",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &tempdir.join(build_dir).join("test.cubin"), // absolute path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2),
            compilations: Some(2),
            compile_requests: Some(1),
            requests_executed: Some(3),
            cache_hits: Some(vec![(CCompilerKind::Cicc, Language::Ptx, 1)]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile A");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(3 + with_debug_flags as u64),
            compilations: Some(4 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile A (cached)");
    run_cuda_test(
        "-c",
        &tempdir.join(INPUT_FOR_CUDA_A), // absolute path for input
        &tempdir.join(build_dir).join(OUTPUT), // absolute path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(!preprocessor_cache_mode as u64),
            compile_requests: Some(1),
            requests_executed: Some(1),
            cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)]),
            preprocessor_cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Compile a copy of `test_a.cu` to ensure we get cache hits for identical PTX across different files.
    trace!("compile A (copy)");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A_COPY), // relative path for input
        &build_dir.join(OUTPUT),          // relative path for output
        &extra_args,
        // Since `test_a_copy.cu` is a copy of `test_a.cu`, its PTX will be identical when *not* using -G.
        // But -G causes cudafe++ and cicc to embed the source path their output, and we get cache misses.
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(3 + with_debug_flags as u64),
            compilations: Some(4 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile A (copy) (cached)");
    run_cuda_test(
        "-c",
        &tempdir.join(INPUT_FOR_CUDA_A_COPY), // absolute path for input
        &tempdir.join(build_dir).join(OUTPUT), // absolute path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(!preprocessor_cache_mode as u64),
            compile_requests: Some(1),
            requests_executed: Some(1),
            cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)]),
            preprocessor_cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile A (internal linkage)");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A_INTERNAL_LINKAGE), // relative path for input
        &build_dir.join(OUTPUT),                      // relative path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(4),
            compilations: Some(5),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_misses: Some(vec![
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile A (internal linkage) (cached)");
    run_cuda_test(
        "-c",
        &tempdir.join(INPUT_FOR_CUDA_A_INTERNAL_LINKAGE), // absolute path for input
        &tempdir.join(build_dir).join(OUTPUT),            // absolute path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(!preprocessor_cache_mode as u64),
            compile_requests: Some(1),
            requests_executed: Some(1),
            cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)]),
            preprocessor_cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // By compiling another input source we verify that the pre-processor
    // phase is correctly running and outputting text
    trace!("compile B");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_B), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(4),
            compilations: Some(5),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile B (cached)");
    run_cuda_test(
        "-c",
        &tempdir.join(INPUT_FOR_CUDA_B), // absolute path for input
        &tempdir.join(build_dir).join(OUTPUT), // absolute path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(!preprocessor_cache_mode as u64),
            compile_requests: Some(1),
            requests_executed: Some(1),
            cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)]),
            preprocessor_cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test to ensure #2299 doesn't regress (https://github.com/mozilla/sccache/issues/2299)
    let test_2299_src_name = "test_2299.cu";
    let test_2299_out_name = "test_2299.cu.o";
    // Two versions of the source with different contents inside the #ifndef __CUDA_ARCH__
    let test_2299_cu_src_1 = r#"
#ifndef __CUDA_ARCH__
static const auto x = 5;
#endif
int main(int argc, char** argv) {
  return 0;
}
"#;
    let test_2299_cu_src_2 = r#"
#ifndef __CUDA_ARCH__
static const auto x = "5";
#endif
int main(int argc, char** argv) {
  return 0;
}
"#;
    write_source(tempdir, test_2299_src_name, test_2299_cu_src_1);
    run_cuda_test(
        "-c",
        Path::new(test_2299_src_name),       // relative path for input
        &build_dir.join(test_2299_out_name), // relative path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(4),
            compilations: Some(5),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    write_source(tempdir, test_2299_src_name, test_2299_cu_src_2);
    trace!("compile test_2299.cu (2)");
    run_cuda_test(
        "-c",
        Path::new(test_2299_src_name),       // relative path for input
        &build_dir.join(test_2299_out_name), // relative path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2),
            compilations: Some(3),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_hits: Some(vec![
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Recompile the original version again to ensure only cache hits
    write_source(tempdir, test_2299_src_name, test_2299_cu_src_1);
    trace!("compile test_2299.cu (1) (cached)");
    run_cuda_test(
        "-c",
        &tempdir.join(test_2299_src_name), // absolute path for input
        &tempdir.join(build_dir).join(test_2299_out_name), // absolute path for output
        &extra_args,
        AdditionalStats {
            preprocessed: Some(!preprocessor_cache_mode as u64),
            compile_requests: Some(1),
            requests_executed: Some(1),
            cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)]),
            preprocessor_cache_hits: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Precompile sm_86 PTX and cubin so their cache entries potentially have a different .module_id file
    trace!("compile A cubin sm_86");
    run_cuda_test(
        "-cubin",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_86,code=[sm_86]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(3),
            compilations: Some(3),
            compile_requests: Some(1),
            requests_executed: Some(3),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Precompile sm_86 PTX and cubin so their cache entries potentially have a different .module_id file
    trace!("compile A (internal linkage) cubin sm_86");
    run_cuda_test(
        "-cubin",
        Path::new(INPUT_FOR_CUDA_A_INTERNAL_LINKAGE), // relative path for input
        &build_dir.join(OUTPUT),                      // relative path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_86,code=[sm_86]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(3),
            compilations: Some(3),
            compile_requests: Some(1),
            requests_executed: Some(3),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test compiling a file whose PTX yields a cache hit for a cubin from another file (`test_a.cu`)
    trace!("compile B cubin sm_86");
    run_cuda_test(
        "-cubin",
        Path::new(INPUT_FOR_CUDA_B), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_86,code=[sm_86]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2 + with_debug_flags as u64),
            compilations: Some(2 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(3),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test compiling a multiarch object where the PTX and cubin for one of the archs is cached
    trace!("compile A sm_80,sm_86");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &[
                "-gencode=arch=compute_80,code=[sm_80]".into(),
                "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
            ],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(5 + with_debug_flags as u64),
            compilations: Some(6 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(7),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 2),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    1 + with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test compiling a multiarch object of a different source file, but
    // whose device code is the same as a previously-compiled files'
    trace!("compile A (copy) sm_80,sm_86");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A_COPY), // relative path for input
        &build_dir.join(OUTPUT),          // relative path for output
        &[
            extra_args.as_slice(),
            &[
                "-gencode=arch=compute_80,code=[sm_80]".into(),
                "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
            ],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(4 + 2 * with_debug_flags as u64),
            compilations: Some(5 + 2 * with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(7),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                2 * !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 2),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    2 * with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test compiling a multiarch object where the PTX and cubin for one of the archs is cached
    trace!("compile A (internal linkage) sm_80,sm_86");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A_INTERNAL_LINKAGE), // relative path for input
        &build_dir.join(OUTPUT),                      // relative path for output
        &[
            extra_args.as_slice(),
            &[
                "-gencode=arch=compute_80,code=[sm_80]".into(),
                "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
            ],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(5 + with_debug_flags as u64),
            compilations: Some(6 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(7),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 2),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    1 + with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile B sm_80,sm_86");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_B), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &[
                "-gencode=arch=compute_80,code=[sm_80]".into(),
                "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
            ],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(5 + with_debug_flags as u64),
            compilations: Some(6 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(7),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
                (CCompilerKind::Cicc, Language::Ptx, 2),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    1 + with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test that compiling a single-arch object where the arch is a subset of
    // a previous multi-arch compilation produces cache hits on the underlying
    // PTX and cubin compilations.
    trace!("compile A sm_80");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_80,code=[compute_80,sm_80]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2),
            compilations: Some(3),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_hits: Some(vec![
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile B sm_80");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_B), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_80,code=[compute_80,sm_80]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2),
            compilations: Some(3),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_hits: Some(vec![
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test that compiling a single-arch cubin where the arch is a subset of
    // a previous multi-arch compilation produces cache hits on the underlying
    // PTX and cubin compilations.
    trace!("compile A cubin sm_80");
    run_cuda_test(
        "-cubin",
        &tempdir.join(INPUT_FOR_CUDA_A), // absolute path for input
        &tempdir.join(build_dir).join("test.cubin"), // absolute path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_80,code=[sm_80]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2 + with_debug_flags as u64),
            compilations: Some(2 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(3),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    trace!("compile B cubin sm_80");
    run_cuda_test(
        "-cubin",
        &tempdir.join(INPUT_FOR_CUDA_B), // absolute path for input
        &tempdir.join(build_dir).join("test.cubin"), // absolute path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_80,code=[sm_80]".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2 + with_debug_flags as u64),
            compilations: Some(2 + with_debug_flags as u64),
            compile_requests: Some(1),
            requests_executed: Some(3),
            cache_hits: Some(vec![(
                CCompilerKind::Ptxas,
                Language::Cubin,
                !with_debug_flags as u64,
            )]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (
                    CCompilerKind::Ptxas,
                    Language::Cubin,
                    with_debug_flags as u64,
                ),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    // Test that compiling sm80 PTX and assembling as an sm86 cubin
    // yields a cache hit for the PTX and cache miss for the cubin
    trace!("compile A compute_80,sm_86");
    run_cuda_test(
        "-c",
        Path::new(INPUT_FOR_CUDA_A), // relative path for input
        &build_dir.join(OUTPUT),     // relative path for output
        &[
            extra_args.as_slice(),
            &["-gencode=arch=compute_80,code=sm_86".into()],
        ]
        .concat(),
        AdditionalStats {
            preprocessed: Some(1),
            cache_writes: Some(2),
            compilations: Some(3),
            compile_requests: Some(1),
            requests_executed: Some(5),
            cache_hits: Some(vec![
                (CCompilerKind::Cicc, Language::Ptx, 1),
                (CCompilerKind::CudaFE, Language::CudaFE, 1),
            ]),
            cache_misses: Some(vec![
                (CCompilerKind::Nvcc, Language::Cuda, 1),
                (CCompilerKind::Ptxas, Language::Cubin, 1),
            ]),
            preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                .filter(|_| preprocessor_cache_mode),
            ..Default::default()
        },
    );

    if !cfg!(target_os = "windows") {
        // Test compiling an executable (`nvcc -x cu test_a.cu -o test_a`)
        trace!("compile A to executable");
        run_cuda_test(
            "",
            Path::new(INPUT_FOR_CUDA_A), // relative path for input
            &build_dir.join("test_a"),   // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[sm_80]".into(),
                    "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(3),
                cache_writes: Some(2),
                compilations: Some(2),
                compile_requests: Some(1),
                non_cacheable_compilations: Some(1),
                requests_executed: Some(8),
                cache_hits: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 2),
                    (CCompilerKind::Ptxas, Language::Cubin, 2),
                ]),
                cache_misses: Some(vec![(host_compiler.name.into(), Language::Cxx, 2)]),
                preprocessor_cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (host_compiler.name.into(), Language::Cxx, 2),
                ])
                .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        // Test compiling an executable (`nvcc -x cu test_a_copy.cu -o test_a_copy`)
        trace!("compile A (copy) to executable");
        run_cuda_test(
            "",
            Path::new(INPUT_FOR_CUDA_A_COPY), // relative path for input
            &build_dir.join("test_a_copy"),   // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[sm_80]".into(),
                    "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(3),
                cache_writes: Some(2),
                compilations: Some(2),
                compile_requests: Some(1),
                non_cacheable_compilations: Some(1),
                requests_executed: Some(8),
                cache_hits: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 2),
                    (CCompilerKind::Ptxas, Language::Cubin, 2),
                ]),
                cache_misses: Some(vec![(host_compiler.name.into(), Language::Cxx, 2)]),
                preprocessor_cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (host_compiler.name.into(), Language::Cxx, 2),
                ])
                .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        // Test compiling an executable (`nvcc -x cu test_a_internal_linkage.cu -o test_a_internal_linkage`)
        trace!("compile A (internal linkage) to executable");
        run_cuda_test(
            "",
            Path::new(INPUT_FOR_CUDA_A_INTERNAL_LINKAGE), // relative path for input
            &build_dir.join("test_a_internal_linkage"),   // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[sm_80]".into(),
                    "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(3),
                cache_writes: Some(2),
                compilations: Some(2),
                compile_requests: Some(1),
                non_cacheable_compilations: Some(1),
                requests_executed: Some(8),
                cache_hits: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 2),
                    (CCompilerKind::Ptxas, Language::Cubin, 2),
                ]),
                cache_misses: Some(vec![(host_compiler.name.into(), Language::Cxx, 2)]),
                preprocessor_cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (host_compiler.name.into(), Language::Cxx, 2),
                ])
                .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        // Test compiling an executable (`nvcc -x cu test_b.cu -o test_b`)
        trace!("compile B to executable");
        run_cuda_test(
            "",
            Path::new(INPUT_FOR_CUDA_B), // relative path for input
            &build_dir.join("test_b"),   // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[sm_80]".into(),
                    "-gencode=arch=compute_86,code=[compute_86,sm_86]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(3),
                cache_writes: Some(2),
                compilations: Some(2),
                compile_requests: Some(1),
                non_cacheable_compilations: Some(1),
                requests_executed: Some(8),
                cache_hits: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 2),
                    (CCompilerKind::Ptxas, Language::Cubin, 2),
                ]),
                cache_misses: Some(vec![(host_compiler.name.into(), Language::Cxx, 2)]),
                preprocessor_cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (host_compiler.name.into(), Language::Cxx, 2),
                ])
                .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );
    }

    // Test lto (CUDA 12.8+)
    if !with_debug_flags
        && with_rdc
        && compiler
            .version()
            .and_then(|nvcc_v| {
                version_compare::compare_to(nvcc_v, "12.8.0", version_compare::Cmp::Ge)
                    .map_err(|_| sccache::errors::anyhow!("nvcc too old"))
            })
            .unwrap_or_default()
    {
        trace!("compile A ltoir");
        run_cuda_test(
            "-ltoir",
            Path::new(INPUT_FOR_CUDA_A),   // relative path for input
            &build_dir.join("test.ltoir"), // relative path for output
            &[
                extra_args.as_slice(),
                // needed to make -ltoir work
                &["-dlto".into()],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(2),
                compilations: Some(2),
                compile_requests: Some(1),
                requests_executed: Some(2),
                cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                ]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );
        // Precompile lto_86 so its cache entry potentially has a different .module_id
        // file when hydrated for other multiarch compilations
        trace!("compile A lto_86");
        run_cuda_test(
            "-ltoir",
            &tempdir.join(INPUT_FOR_CUDA_A), // absolute path for input
            &tempdir.join(build_dir).join("test.ltoir"), // absolute path for output
            &[
                extra_args.as_slice(),
                &[
                    "-arch=compute_86".into(),
                    "-code=lto_86".into(),
                    // needed to make -ltoir work
                    "-dlto".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(2),
                compilations: Some(2),
                compile_requests: Some(1),
                requests_executed: Some(2),
                cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                ]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        // Test compiling a multiarch object
        trace!("compile A lto_80,lto_86");
        run_cuda_test(
            "-c",
            Path::new(INPUT_FOR_CUDA_A), // relative path for input
            &build_dir.join(OUTPUT),     // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[lto_80]".into(),
                    "-gencode=arch=compute_86,code=[lto_86]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(3),
                compilations: Some(4),
                compile_requests: Some(1),
                requests_executed: Some(5),
                cache_hits: Some(vec![(CCompilerKind::CudaFE, Language::CudaFE, 1)]),
                cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 2),
                ]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        // Test compiling a multiarch object and embedding PTX where the LtoIR for one of the archs is cached
        trace!("compile A [lto_80] [compute_86,lto_86]");
        run_cuda_test(
            "-c",
            Path::new(INPUT_FOR_CUDA_A), // relative path for input
            &build_dir.join(OUTPUT),     // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[lto_80]".into(),
                    "-gencode=arch=compute_86,code=[compute_86,lto_86]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(2),
                compilations: Some(3),
                compile_requests: Some(1),
                requests_executed: Some(5),
                cache_hits: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                ]),
                cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                ]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        // Test compiling a multiarch object and embedding PTX where the LtoIR for one of the archs is cached
        trace!("compile A arch=lto_80,code=lto_80");
        run_cuda_test(
            "-dc",
            Path::new(INPUT_FOR_CUDA_A), // relative path for input
            Path::new("test.ltoir.o"),   // relative path for output
            &[
                extra_args.as_slice(),
                &["-gencode=arch=lto_80,code=lto_80".into()],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(1),
                compilations: Some(2),
                compile_requests: Some(1),
                requests_executed: Some(4),
                cache_hits: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                ]),
                cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );
    }

    // Test Blackwell arch modifiers (CUDA 13.1+)
    if compiler
        .version()
        .and_then(|nvcc_v| {
            version_compare::compare_to(nvcc_v, "13.1.0", version_compare::Cmp::Ge)
                .map_err(|_| sccache::errors::anyhow!("nvcc too old"))
        })
        .unwrap_or_default()
    {
        trace!("compile A sm_120f+uumn");
        run_cuda_test(
            "-c",
            Path::new(INPUT_FOR_CUDA_A), // relative path for input
            &build_dir.join(OUTPUT),     // relative path for output
            &[
                extra_args.as_slice(),
                &["-gencode=arch=compute_120f+uumn,code=[compute_120f+uumn,sm_120f+uumn]".into()],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(4),
                compilations: Some(5),
                compile_requests: Some(1),
                requests_executed: Some(5),
                cache_misses: Some(vec![
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                    (CCompilerKind::Ptxas, Language::Cubin, 1),
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                ]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );

        trace!("compile A sm_80,sm_120f+uumn");
        run_cuda_test(
            "-c",
            Path::new(INPUT_FOR_CUDA_A), // relative path for input
            &build_dir.join(OUTPUT),     // relative path for output
            &[
                extra_args.as_slice(),
                &[
                    "-gencode=arch=compute_80,code=[sm_80]".into(),
                    "-gencode=arch=compute_120f+uumn,code=[compute_120f+uumn,sm_120f+uumn]".into(),
                ],
            ]
            .concat(),
            AdditionalStats {
                preprocessed: Some(1),
                cache_writes: Some(3 + with_debug_flags as u64),
                compilations: Some(4 + with_debug_flags as u64),
                compile_requests: Some(1),
                requests_executed: Some(7),
                cache_hits: Some(vec![
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                    (
                        CCompilerKind::Ptxas,
                        Language::Cubin,
                        1 + !with_debug_flags as u64,
                    ),
                ]),
                cache_misses: Some(vec![
                    (CCompilerKind::Nvcc, Language::Cuda, 1),
                    (CCompilerKind::CudaFE, Language::CudaFE, 1),
                    (CCompilerKind::Cicc, Language::Ptx, 1),
                    (
                        CCompilerKind::Ptxas,
                        Language::Cubin,
                        with_debug_flags as u64,
                    ),
                ]),
                preprocessor_cache_misses: Some(vec![(CCompilerKind::Nvcc, Language::Cuda, 1)])
                    .filter(|_| preprocessor_cache_mode),
                ..Default::default()
            },
        );
    }
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn test_nvcc_proper_lang_stat_tracking(
    client: &SccacheClient,
    compiler: &Compiler,
    host_compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
    with_debug_flags: bool,
    with_rdc: bool,
) {
    use sccache::{
        compiler::{CCompilerKind, CompilerKind, Language},
        server::ServerStats,
    };

    let mut stats = client.stats().unwrap();

    let mut extra_args = vec![format!("-ccbin={}", host_compiler.exe.to_string_lossy()).into()];

    if with_debug_flags {
        extra_args.push("--device-debug".into());
    }

    if with_rdc {
        extra_args.push("-rdc=true".into());
    }

    extra_args.push("-t=10".into());

    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;

    println!(
        "test_nvcc_proper_lang_stat_tracking: {name}/{}",
        host_compiler.name
    );
    // Compile multiple source files.
    copy_to_tempdir(&[INPUT_FOR_CUDA_C, INPUT], tempdir);

    let out_file = tempdir.join(OUTPUT);

    trace!("compile CUDA C");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT_FOR_CUDA_C,
            OUTPUT,
            extra_args.clone(),
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();

    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();

    stats.cache_writes += 4;
    stats.preprocessed += 1;
    stats.compilations += 5;
    stats.compile_requests += 1;
    stats.requests_executed += 5;
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::Cuda);
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::CudaFE), &Language::CudaFE);
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Cicc), &Language::Ptx);
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Ptxas), &Language::Cubin);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_misses
            .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::Cuda);
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile CUDA C");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT_FOR_CUDA_C,
            OUTPUT,
            extra_args.clone(),
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();

    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();

    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_hits
        .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_hits
            .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::Cuda);
    } else {
        stats.preprocessed += 1;
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile C");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT,
            OUTPUT,
            extra_args.clone(),
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();

    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();

    stats.cache_writes += 1;
    stats.preprocessed += 1;
    stats.compilations += 2;
    stats.compile_requests += 1;
    stats.requests_executed += 2;
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::C);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_misses
            .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::C);
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile C");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT,
            OUTPUT,
            extra_args.clone(),
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();

    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();

    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_hits
        .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::C);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_hits
            .increment(&CompilerKind::C(CCompilerKind::Nvcc), &Language::C);
    } else {
        stats.preprocessed += 1;
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn run_sccache_nvcc_cuda_command_tests(
    client: &SccacheClient,
    compiler: &Compiler,
    host_compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
    with_debug_flags: bool,
    with_rdc: bool,
) {
    test_nvcc_cuda_compiles(
        client,
        compiler,
        host_compiler,
        tempdir,
        preprocessor_cache_mode,
        with_debug_flags,
        with_rdc,
    );
    test_nvcc_proper_lang_stat_tracking(
        client,
        compiler,
        host_compiler,
        tempdir,
        preprocessor_cache_mode,
        with_debug_flags,
        with_rdc,
    );
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn test_clang_cuda_compiles(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
    with_debug_flags: bool,
    with_rdc: bool,
) {
    use sccache::{
        compiler::{CCompilerKind, CompilerKind, Language},
        server::ServerStats,
    };

    let mut stats = client.stats().unwrap();

    let mut extra_args = vec![];

    if with_debug_flags {
        extra_args.push("-g".into());
        extra_args.push("--cuda-noopt-device-debug".into());
    }

    if with_rdc {
        extra_args.push("-fgpu-rdc".into());
    }

    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_clang_cuda_compiles: {name}");
    // Compile multiple source files.
    copy_to_tempdir(&[INPUT_FOR_CUDA_A, INPUT_FOR_CUDA_B], tempdir);

    let out_file = tempdir.join(OUTPUT);
    trace!("compile A");
    client
        .cmd()
        .args(compile_cuda_cmdline(
            name,
            exe,
            "-c",
            INPUT_FOR_CUDA_A,
            OUTPUT,
            &extra_args,
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();
    stats.cache_writes += 1;
    stats.preprocessed += 1;
    stats.compilations += 1;
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_misses
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile A");
    client
        .cmd()
        .args(compile_cuda_cmdline(
            name,
            exe,
            "-c",
            INPUT_FOR_CUDA_A,
            OUTPUT,
            &extra_args,
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_hits
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_hits
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    } else {
        stats.preprocessed += 1;
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    // By compiling another input source we verify that the pre-processor
    // phase is correctly running and outputting text
    trace!("compile B");
    client
        .cmd()
        .args(compile_cuda_cmdline(
            name,
            exe,
            "-c",
            INPUT_FOR_CUDA_B,
            OUTPUT,
            &extra_args,
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();
    stats.cache_writes += 1;
    stats.preprocessed += 1;
    stats.compilations += 1;
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_misses
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile B");
    client
        .cmd()
        .args(compile_cuda_cmdline(
            name,
            exe,
            "-c",
            INPUT_FOR_CUDA_B,
            OUTPUT,
            &extra_args,
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    fs::remove_file(&out_file).unwrap();
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_hits
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_hits
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    } else {
        stats.preprocessed += 1;
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn test_clang_proper_lang_stat_tracking(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
    with_debug_flags: bool,
    with_rdc: bool,
) {
    use sccache::{
        compiler::{CCompilerKind, CompilerKind, Language},
        server::ServerStats,
    };

    let mut stats = client.stats().unwrap();

    let mut extra_args = vec![];

    if with_debug_flags {
        extra_args.push("-g".into());
        extra_args.push("--cuda-noopt-device-debug".into());
    }

    if with_rdc {
        extra_args.push("-fgpu-rdc".into());
    }

    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;

    println!("test_clang_proper_lang_stat_tracking: {name}");
    // Compile multiple source files.
    copy_to_tempdir(&[INPUT_FOR_CUDA_C, INPUT], tempdir);

    let out_file = tempdir.join(OUTPUT);
    trace!("compile CUDA A");
    client
        .cmd()
        .args(compile_cuda_cmdline(
            name,
            exe,
            "-c",
            INPUT_FOR_CUDA_C,
            OUTPUT,
            &extra_args,
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    fs::remove_file(&out_file).unwrap();
    stats.cache_writes += 1;
    stats.preprocessed += 1;
    stats.compilations += 1;
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_misses
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile CUDA A");
    client
        .cmd()
        .args(compile_cuda_cmdline(
            name,
            exe,
            "-c",
            INPUT_FOR_CUDA_C,
            OUTPUT,
            &extra_args,
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    fs::remove_file(&out_file).unwrap();
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_hits
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_hits
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cuda);
    } else {
        stats.preprocessed += 1;
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile C++ A");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT,
            OUTPUT,
            extra_args.clone(),
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    fs::remove_file(&out_file).unwrap();
    stats.cache_writes += 1;
    stats.preprocessed += 1;
    stats.compilations += 1;
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_misses
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cxx);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_misses
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cxx);
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );

    trace!("compile C++ A");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT,
            OUTPUT,
            extra_args.clone(),
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    fs::remove_file(&out_file).unwrap();
    stats.compile_requests += 1;
    stats.requests_executed += 1;
    stats
        .cache_hits
        .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cxx);
    if preprocessor_cache_mode {
        stats
            .preprocessor_cache_hits
            .increment(&CompilerKind::C(CCompilerKind::Clang), &Language::Cxx);
    } else {
        stats.preprocessed += 1;
    }
    assert_eq!(
        stats,
        ServerStats {
            cache_write_duration: stats.cache_write_duration,
            cache_read_hit_duration: stats.cache_read_hit_duration,
            compiler_write_duration: stats.compiler_write_duration,
            preprocessor_duration: stats.preprocessor_duration,
            preprocessor_cache_hit_duration: stats.preprocessor_cache_hit_duration,
            preprocessor_cache_miss_duration: stats.preprocessor_cache_miss_duration,
            ..client.stats().unwrap()
        }
    );
}

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
fn run_sccache_clang_cuda_command_tests(
    client: &SccacheClient,
    compiler: &Compiler,
    _host_compiler: &Compiler,
    tempdir: &Path,
    preprocessor_cache_mode: bool,
    with_debug_flags: bool,
    with_rdc: bool,
) {
    test_clang_cuda_compiles(
        client,
        compiler,
        tempdir,
        preprocessor_cache_mode,
        with_debug_flags,
        with_rdc,
    );
    test_clang_proper_lang_stat_tracking(
        client,
        compiler,
        tempdir,
        preprocessor_cache_mode,
        with_debug_flags,
        with_rdc,
    );
}

fn test_hip_compiles(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_hip_compiles: {name}");
    // Compile multiple source files.
    copy_to_tempdir(&[INPUT_FOR_HIP_A, INPUT_FOR_HIP_B], tempdir);

    let target_arch = vec!["gfx900".to_string()];

    let out_file = tempdir.join(OUTPUT);
    trace!("compile A");
    client
        .cmd()
        .args(compile_hip_cmdline(
            name,
            exe,
            INPUT_FOR_HIP_A,
            OUTPUT,
            &target_arch,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.compile_requests);
    assert_eq!(1, stats.requests_executed);
    assert_eq!(0, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_misses.get("HIP").unwrap());
    let adv_hip_key = adv_key_kind("hip", compiler.name);
    assert_eq!(&1, stats.cache_misses.get_adv(&adv_hip_key).unwrap());
    trace!("compile A");
    fs::remove_file(&out_file).unwrap();
    client
        .cmd()
        .args(compile_hip_cmdline(
            name,
            exe,
            INPUT_FOR_HIP_A,
            OUTPUT,
            &target_arch,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(2, stats.compile_requests);
    assert_eq!(2, stats.requests_executed);
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("HIP").unwrap());
    assert_eq!(&1, stats.cache_misses.get("HIP").unwrap());
    let adv_hip_key = adv_key_kind("hip", compiler.name);
    assert_eq!(&1, stats.cache_hits.get_adv(&adv_hip_key).unwrap());
    assert_eq!(&1, stats.cache_misses.get_adv(&adv_hip_key).unwrap());
    // By compiling another input source we verify that the pre-processor
    // phase is correctly running and outputting text
    trace!("compile B");
    client
        .cmd()
        .args(compile_hip_cmdline(
            name,
            exe,
            INPUT_FOR_HIP_B,
            OUTPUT,
            &target_arch,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(3, stats.compile_requests);
    assert_eq!(3, stats.requests_executed);
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(2, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("HIP").unwrap());
    assert_eq!(&2, stats.cache_misses.get("HIP").unwrap());
    let adv_hip_key = adv_key_kind("hip", compiler.name);
    assert_eq!(&1, stats.cache_hits.get_adv(&adv_hip_key).unwrap());
    assert_eq!(&2, stats.cache_misses.get_adv(&adv_hip_key).unwrap());
}

fn test_hip_compiles_multi_targets(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_hip_compiles_multi_targets: {name}");
    // Compile multiple source files.
    copy_to_tempdir(&[INPUT_FOR_HIP_A, INPUT_FOR_HIP_B], tempdir);

    let target_arches: Vec<String> = vec!["gfx900".to_string(), "gfx1030".to_string()];

    let out_file = tempdir.join(OUTPUT);
    trace!("compile A with gfx900 and gfx1030");
    client
        .cmd()
        .args(compile_hip_cmdline(
            name,
            exe,
            INPUT_FOR_HIP_A,
            OUTPUT,
            &target_arches,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.compile_requests);
    assert_eq!(1, stats.requests_executed);
    assert_eq!(0, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_misses.get("HIP").unwrap());
    let adv_hip_key = adv_key_kind("hip", compiler.name);
    assert_eq!(&1, stats.cache_misses.get_adv(&adv_hip_key).unwrap());

    trace!("compile A with with gfx900 and gfx1030 again");
    fs::remove_file(&out_file).unwrap();
    client
        .cmd()
        .args(compile_hip_cmdline(
            name,
            exe,
            INPUT_FOR_HIP_A,
            OUTPUT,
            &target_arches,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(2, stats.compile_requests);
    assert_eq!(2, stats.requests_executed);
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("HIP").unwrap());
    assert_eq!(&1, stats.cache_misses.get("HIP").unwrap());
    let adv_hip_key = adv_key_kind("hip", compiler.name);
    assert_eq!(&1, stats.cache_hits.get_adv(&adv_hip_key).unwrap());
    assert_eq!(&1, stats.cache_misses.get_adv(&adv_hip_key).unwrap());

    // By compiling another input source we verify that the pre-processor
    // phase is correctly running and outputting text
    trace!("compile B with gfx900 and gfx1030");
    client
        .cmd()
        .args(compile_hip_cmdline(
            name,
            exe,
            INPUT_FOR_HIP_B,
            OUTPUT,
            &target_arches,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    assert!(fs::metadata(&out_file).map(|m| m.len() > 0).unwrap());
    trace!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(3, stats.compile_requests);
    assert_eq!(3, stats.requests_executed);
    assert_eq!(1, stats.cache_hits.all());
    assert_eq!(2, stats.cache_misses.all());
    assert_eq!(&1, stats.cache_hits.get("HIP").unwrap());
    assert_eq!(&2, stats.cache_misses.get("HIP").unwrap());
    let adv_hip_key = adv_key_kind("hip", compiler.name);
    assert_eq!(&1, stats.cache_hits.get_adv(&adv_hip_key).unwrap());
    assert_eq!(&2, stats.cache_misses.get_adv(&adv_hip_key).unwrap());
}

fn run_sccache_hip_command_tests(client: &SccacheClient, compiler: Compiler, tempdir: &Path) {
    client.zero_stats();
    test_hip_compiles(client, &compiler, tempdir);
    client.zero_stats();
    test_hip_compiles_multi_targets(client, &compiler, tempdir);
    // test_proper_lang_stat_tracking(compiler, tempdir);
}

fn test_clang_multicall(client: &SccacheClient, compiler: &Compiler, tempdir: &Path) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_clang_multicall: {name}");
    // Compile a source file.
    copy_to_tempdir(&[INPUT_CLANG_MULTICALL], tempdir);

    println!("compile clang_multicall");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT_CLANG_MULTICALL,
            OUTPUT,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
}

fn test_clang_cache_whitespace_normalization(
    client: &SccacheClient,
    compiler: &Compiler,
    tempdir: &Path,
    hit: bool,
) {
    let Compiler {
        name,
        exe,
        env_vars,
        ..
    } = compiler;
    println!("test_clang_cache_whitespace_normalization: {name}");
    debug!("expecting hit: {hit}");
    // Compile a source file.
    copy_to_tempdir(&[INPUT_WITH_WHITESPACE, INPUT_WITH_WHITESPACE_ALT], tempdir);
    client.zero_stats();

    debug!("compile whitespace");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT_WITH_WHITESPACE,
            OUTPUT,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    debug!("request stats");
    let stats = client.stats().unwrap();
    assert_eq!(1, stats.compile_requests);
    assert_eq!(1, stats.requests_executed);
    assert_eq!(0, stats.cache_hits.all());
    assert_eq!(1, stats.cache_misses.all());

    debug!("compile whitespace_alt");
    client
        .cmd()
        .args(compile_cmdline(
            name,
            exe,
            INPUT_WITH_WHITESPACE_ALT,
            OUTPUT,
            [],
        ))
        .current_dir(tempdir)
        .envs(env_vars.clone())
        .assert()
        .success();
    debug!("request stats (expecting cache hit)");
    if hit {
        let stats = client.stats().unwrap();
        assert_eq!(2, stats.compile_requests);
        assert_eq!(2, stats.requests_executed);
        assert_eq!(1, stats.cache_hits.all());
        assert_eq!(1, stats.cache_misses.all());
    } else {
        let stats = client.stats().unwrap();
        assert_eq!(2, stats.compile_requests);
        assert_eq!(2, stats.requests_executed);
        assert_eq!(0, stats.cache_hits.all());
        assert_eq!(2, stats.cache_misses.all());
    }
}

// We detect the HIP Clang compiler through 2 methods:
// 1. If the env var HIP_CLANG_PATH is set, try $HIP_CLANG_PATH/clang. This is the same behavior as
//    hipcc, but is rarely know, so we have another option.
// 2. If the env var ROCM_PATH is set, try $ROCM_PATH/llvm/bin/clang. This is the location in
//    AMD's official debian packages.
// 3. Otherwise, just bail.
fn find_hip_compiler() -> Option<Compiler> {
    let env_vars: Vec<(OsString, OsString)> = env::vars_os().collect();

    if let Ok(hip_clang_path) = env::var("HIP_CLANG_PATH") {
        let clang_path = Path::new(&hip_clang_path).join("clang");

        if let Ok(true) = clang_path.try_exists() {
            return Some(Compiler {
                name: "clang",
                exe: clang_path.as_path().into(),
                env_vars,
            });
        }
    }
    if let Ok(rocm_path) = env::var("ROCM_PATH") {
        let clang_path = Path::new(&rocm_path).join("llvm").join("bin").join("clang");

        if let Ok(true) = clang_path.try_exists() {
            return Some(Compiler {
                name: "hip",
                exe: clang_path.as_path().into(),
                env_vars,
            });
        }
    }
    None
}

#[test]
fn test_stats_no_server() {
    let client = SccacheClient::new_no_cfg();
    let _ = client.stats();
    assert!(
        !client.stop(),
        "Server shouldn't be running after --show-stats"
    );
}

macro_rules! test_sccache_command {
    ($compiler:ident, $compiler_name:expr) => {
        paste! {
            #[test_case(true ; "with preprocessor cache")]
            #[test_case(false ; "without preprocessor cache")]
            #[cfg(any(unix, target_env = "msvc"))]
            fn [<test_sccache_command_ $compiler>] (
                preprocessor_cache_mode: bool
            ) {
                let _ = env_logger::try_init();
                let compilers = find_compilers();
                if let Some(compiler) = compilers.iter().find(|c| c.name == $compiler_name) {
                    // Create and start the sccache client
                    let (_tempdir, tempdir_path, client) = make_sccache_client(preprocessor_cache_mode);
                    run_sccache_command_tests(
                        &client.start(),
                        &compiler,
                        &tempdir_path,
                        preprocessor_cache_mode,
                    )
                }
            }
        }
    };
}

#[cfg(unix)]
test_sccache_command!(gcc, "gcc");
#[cfg(unix)]
test_sccache_command!(clang, "clang");
#[cfg(unix)]
test_sccache_command!(clangxx, "clang++");
#[cfg(unix)]
test_sccache_command!(nvc, "nvc");
#[cfg(unix)]
test_sccache_command!(nvcxx, "nvc++");

#[cfg(target_env = "msvc")]
test_sccache_command!(msvc, "cl");

#[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
macro_rules! test_cuda_sccache_command {
    ($cuda_compiler:ident, $host_compiler:ident, $cuda_compiler_name:expr, $host_compiler_name:expr) => {
        paste! {
            #[test_case(false, false, false ; "preprocessor_cache=false, device_debug=false, rdc=false")]
            #[test_case(false, false, true ; "preprocessor_cache=false, device_debug=false, rdc=true")]
            #[test_case(false, true, false ; "preprocessor_cache=false, device_debug=true, rdc=false")]
            #[test_case(false, true, true ; "preprocessor_cache=false, device_debug=true, rdc=true")]
            #[test_case(true, false, false ; "preprocessor_cache=true, device_debug=false, rdc=false")]
            #[test_case(true, false, true ; "preprocessor_cache=true, device_debug=false, rdc=true")]
            #[test_case(true, true, false ; "preprocessor_cache=true, device_debug=true, rdc=false")]
            #[test_case(true, true, true ; "preprocessor_cache=true, device_debug=true, rdc=true")]
            fn [<test_cuda_sccache_command_ $cuda_compiler _ $host_compiler>] (
                preprocessor_cache_mode: bool,
                with_debug_flags: bool,
                with_rdc: bool,
            ) {
                let _ = env_logger::try_init();
                let cuda_compilers = harness::find_cuda_compilers();
                if let Some(cuda_compiler) = cuda_compilers.iter().find(|c| c.name == $cuda_compiler_name) {
                    let host_compilers = find_compilers();
                    if let Some(host_compiler) = host_compilers.iter().find(|c| c.name == $host_compiler_name) {
                        // Create and start the sccache client
                        let (_tempdir, tempdir_path, client) = make_sccache_client(preprocessor_cache_mode);
                        [<run_sccache_ $cuda_compiler _cuda_command_tests>](
                            &client.start(),
                            &cuda_compiler,
                            &host_compiler,
                            &tempdir_path,
                            preprocessor_cache_mode,
                            with_debug_flags,
                            with_rdc,
                        )
                    }
                }
            }
        }
    };
}

// Linux
#[cfg(target_os = "linux")]
test_cuda_sccache_command!(nvcc, gcc, "nvcc", "gcc");
#[cfg(target_os = "linux")]
test_cuda_sccache_command!(nvcc, clang, "nvcc", "clang++");
#[cfg(target_os = "linux")]
test_cuda_sccache_command!(nvcc, nvcxx, "nvcc", "nvc++");
#[cfg(target_os = "linux")]
test_cuda_sccache_command!(clang, clang, "clang++", "clang++");

// Windows
#[cfg(target_env = "msvc")]
test_cuda_sccache_command!(nvcc, cl, "nvcc", "cl");
#[cfg(target_env = "msvc")]
test_cuda_sccache_command!(clang, clang, "clang++", "clang++");

#[test_case(true ; "with preprocessor cache")]
#[test_case(false ; "without preprocessor cache")]
#[cfg(any(unix, target_env = "msvc"))]
fn test_hip_sccache_command(preprocessor_cache_mode: bool) {
    let _ = env_logger::try_init();

    if let Some(compiler) = find_hip_compiler() {
        // Create and start the sccache client
        let (_tempdir, tempdir_path, client) = make_sccache_client(preprocessor_cache_mode);
        run_sccache_hip_command_tests(&client.start(), compiler, &tempdir_path);
    }
}

// Test that invoking (e.g.) a symlink called "gcc" to "sccache" results in sccache invoking gcc
#[test]
#[serial]
#[cfg(unix)]
fn test_symlinked_exe() {
    let compilers = find_compilers();
    if compilers.is_empty() {
        warn!("No compilers found, skipping test");
        return;
    }

    let (_tempdir, tempdir_path, client) = make_sccache_client(true);

    let get_version = |compiler: &mut Command| {
        compiler
            .arg("--version")
            .current_dir(&tempdir_path)
            .output()
            .unwrap()
            .stdout
    };

    let mut cmd = client.cmd();
    let sccache_path = cmd.get_program().to_os_string();
    let sccache_version = get_version(&mut cmd);

    for compiler in compilers {
        trace!(
            "Testing sccache symlink to {} ({:?})",
            compiler.name, compiler.exe
        );
        let compiler_version =
            get_version(Command::new(&compiler.exe).envs(compiler.env_vars.clone()));
        assert_ne!(compiler_version, sccache_version);

        let exe_filename = Path::new(&compiler.exe).file_name().unwrap();
        let sccache_compiler_alias = tempdir_path.as_path().join(exe_filename);
        unix_fs::symlink(&sccache_path, &sccache_compiler_alias).unwrap();

        let compiler_alias_version =
            get_version(Command::new(&sccache_compiler_alias).envs(compiler.env_vars));
        assert_eq!(compiler_version, compiler_alias_version);

        fs::remove_file(&sccache_compiler_alias).unwrap();
    }
}
