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

use crate::{dist, errors::*};
use async_trait::async_trait;
use fs_err as fs;
use std::{
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(all(
    feature = "dist-client",
    any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "windows",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        target_os = "freebsd"
    )
))]
pub use self::toolchain_imp::*;

#[async_trait]
pub trait ToolchainPackager: Send + Sync {
    async fn package(
        self: Box<Self>,
        path_transformer: &mut dist::PathTransformer,
    ) -> Result<Arc<dyn PackagedToolchain>>;
}

#[async_trait]
pub trait PackagedToolchain: Send + Sync {
    async fn compute_hash(&self) -> Result<String>;
    async fn write_tar_gz(
        &self,
        toolchain: &dist::Toolchain,
        writer: &mut (dyn Write + Send),
    ) -> Result<()>;
}

pub trait InputsWriter: Write + Send {
    fn finish(self: Box<Self>) -> Result<(u64, u64)>;
}

pub struct InputsCompressor<W: Write + Send> {
    inner: flate2::write::ZlibEncoder<W>,
}

impl<W: Write + Send> InputsCompressor<W> {
    pub fn new(compressor: flate2::write::ZlibEncoder<W>) -> Box<Self> {
        Box::new(Self { inner: compressor })
    }
}

impl<W: Write + Send> Write for InputsCompressor<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write + Send> InputsWriter for InputsCompressor<W> {
    fn finish(self: Box<Self>) -> Result<(u64, u64)> {
        let mut inner = self.inner;
        inner.flush().context("failed to flush compressor")?;
        let total_in = inner.total_in();
        let total_out = inner.total_out();
        trace!("Compressed inputs from {total_in} -> {total_out}");
        inner.finish().context("failed to finish compressor")?;
        Ok((total_in, total_out))
    }
}

/// One thing a job needs staged, named by where it must appear in the job's
/// filesystem.
///
/// This is the same information a tar entry carries, minus the bytes: a
/// `File` says *where to find* the content rather than embedding a copy of
/// it. That distinction is the entire point. A client that content-addresses
/// each input separately can hash the file in place -- and, crucially, can
/// remember that hash across jobs -- whereas an archive forces every byte to
/// be copied through it on every single job, which defeats any memoization.
///
/// Paths are the same root-relative form [`tar_safe_path`] produces, so the
/// two representations describe identical trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEntry {
    /// A file that exists on this machine and can be read on demand.
    File {
        dist_path: PathBuf,
        src_path: PathBuf,
    },
    /// Content that exists only in memory. Preprocessed source is the only
    /// producer, so this variant disappears with the preprocessed staging mode.
    Blob {
        dist_path: PathBuf,
        data: Vec<u8>,
    },
    Symlink {
        dist_path: PathBuf,
        target: PathBuf,
    },
    Dir {
        dist_path: PathBuf,
    },
}

#[async_trait]
pub trait InputsPackager: Send {
    async fn write_inputs(
        self: Box<Self>,
        path_transformer: &mut dist::PathTransformer,
        inputs_writer: Box<dyn InputsWriter>,
    ) -> Result<()>;

    /// Can this packager enumerate its inputs without building an archive?
    ///
    /// Asked before [`Self::list_inputs`] because both it and `write_inputs`
    /// consume the packager, so the choice has to be made up front.
    fn can_list_inputs(&self) -> bool {
        false
    }

    /// Enumerate the inputs directly. Only called when
    /// [`Self::can_list_inputs`] is true.
    async fn list_inputs(
        self: Box<Self>,
        _path_transformer: &mut dist::PathTransformer,
    ) -> Result<Vec<InputEntry>> {
        bail!("This packager cannot enumerate inputs without an archive")
    }
}

#[cfg(not(all(
    feature = "dist-client",
    any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "windows",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        target_os = "freebsd"
    )
)))]
mod toolchain_imp {
    use std::sync::Arc;

    use super::{PackagedToolchain, ToolchainPackager, dist};
    use async_trait::async_trait;

    use crate::errors::*;

    // Distributed client, but an unsupported platform for toolchain packaging so
    // create a failing implementation that will conflict with any others.
    #[async_trait]
    impl<T: Send + Sync> ToolchainPackager for T {
        async fn package(
            self: Box<Self>,
            _: &mut dist::PathTransformer,
        ) -> Result<Arc<dyn PackagedToolchain>> {
            bail!("Automatic packaging not supported on this platform")
        }
    }
}

#[cfg(all(
    feature = "dist-client",
    any(
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "windows",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        target_os = "freebsd"
    )
))]
mod toolchain_imp {
    use async_trait::async_trait;
    use fs_err as fs;
    use is_executable::IsExecutable;
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::OsString;
    use std::io::Write;
    use std::path::{Component, Path, PathBuf};
    use std::process;
    use std::str;
    use std::sync::Arc;
    use walkdir::WalkDir;

    use super::{SimplifyPath, dist, tar_safe_path};
    use crate::{
        errors::*,
        util::{bytes_to_string, path_to_bytes},
    };

    pub struct ToolchainPackaged<'a> {
        executable: PathBuf,
        // Put dirs and file in a deterministic order (map from tar_path -> real_path)
        dirs_set: BTreeSet<PathBuf>,
        file_set: BTreeSet<PathBuf>,
        // Symlinks to add to the tar
        // These are _not_ tar safe, and must be made so before being added to the tar (see
        // `tar_safe_path`).
        symlinks: BTreeMap<PathBuf, PathBuf>,
        path_transformer: &'a mut dist::PathTransformer,
    }

    impl<'a> ToolchainPackaged<'a> {
        pub fn new(executable: PathBuf, path_transformer: &'a mut dist::PathTransformer) -> Self {
            Self {
                executable,
                dirs_set: BTreeSet::new(),
                file_set: BTreeSet::new(),
                symlinks: BTreeMap::new(),
                path_transformer,
            }
        }

        pub fn add_executable_and_deps<P: AsRef<Path>>(
            &mut self,
            env_vars: &[(OsString, OsString)],
            executable: P,
        ) -> Result<()> {
            let executable = executable.as_ref();
            let mut remaining = vec![executable.to_owned()];
            while let Some(obj_path) = remaining.pop() {
                assert!(obj_path.is_absolute());
                // If any parent directories are a symlink, resolve it first and record the link.
                // This is important because ld-linux may not be configured to look in the resolved
                // or non-resolved directory (i.e., both directories must work at runtime).
                //
                let obj_path = self.simplify_path(&obj_path)?;
                // If file already in the set, assume we've analysed all deps
                if self.file_set.contains(&obj_path) {
                    continue;
                }
                let ldd_libraries = find_ldd_libraries(env_vars, &obj_path).with_context(|| {
                    format!("Failed to analyse {} with ldd", obj_path.display())
                })?;
                remaining.extend(ldd_libraries);
                trace!("add_executable_and_deps {}", obj_path.display());
                self.file_set.insert(obj_path);
            }
            Ok(())
        }

        pub fn add_dir<P: AsRef<Path>>(&mut self, dir_path: P) -> Result<()> {
            let dir_path = dir_path.as_ref();
            assert!(dir_path.is_absolute());
            if !dir_path.is_dir() {
                bail!(format!(
                    "{} was not a dir when readying for tar",
                    dir_path.display()
                ))
            }
            if dir_path
                .components()
                .next_back()
                .expect("asserted absolute")
                == Component::RootDir
            {
                return Ok(());
            }
            let dir_path = self.simplify_path(dir_path)?;
            trace!("add_dir {}", dir_path.display());
            self.dirs_set.insert(dir_path);
            Ok(())
        }

        pub fn add_file<P: AsRef<Path>>(
            &mut self,
            env_vars: &[(OsString, OsString)],
            file_path: P,
        ) -> Result<()> {
            let file_path = file_path.as_ref();
            assert!(file_path.is_absolute());
            if !file_path.is_file() {
                bail!(format!(
                    "{} was not a file when readying for tar",
                    file_path.display()
                ))
            }
            if file_path.is_executable()
                && self.add_executable_and_deps(env_vars, file_path).is_ok()
            {
                return Ok(());
            }
            let file_path = self.simplify_path(file_path)?;
            trace!("add_file {}", file_path.display());
            self.file_set.insert(file_path);
            Ok(())
        }

        pub fn add_link<P: AsRef<Path>>(&mut self, target: P, name: P) -> Result<()> {
            let target = target.as_ref();
            let name = name.as_ref();
            assert!(target.is_absolute());
            assert!(name.is_absolute());

            let mut simplify = |path: &Path| -> Result<PathBuf> {
                if path.is_symlink()
                    && let Some(name) = path.file_name()
                    && let Some(path) = path.parent()
                {
                    self.simplify_path(path).map(|path| path.join(name))
                } else {
                    self.simplify_path(path)
                }
            };

            // Simplify the link path
            let p = simplify(target)?;
            // Simplify the link name to record any symlinks it traverses,
            // but write the original name as the actual link name in the archive.
            let _ = simplify(name)?;
            trace!("add_link {} -> {}", p.display(), name.display());
            self.symlinks.insert(p, name.to_path_buf());
            Ok(())
        }

        pub fn add_dir_contents<P: AsRef<Path>>(
            &mut self,
            env_vars: &[(OsString, OsString)],
            dir_path: P,
        ) -> Result<()> {
            let dir_path = dir_path.as_ref();
            // Although by not following symlinks we could break a custom
            // constructed toolchain with links everywhere, this is just a
            // best-effort auto packaging
            for entry in WalkDir::new(dir_path).follow_links(false) {
                let entry = entry?;
                let path = entry.path();
                let file_type = entry.file_type();
                if file_type.is_dir() {
                    continue;
                } else if file_type.is_symlink() {
                    // Skip symlinks that point to nothing
                    if !path.exists() {
                        continue;
                    }
                    let metadata = fs::metadata(path)?;
                    if !metadata.file_type().is_file() {
                        continue;
                    }
                } else if !file_type.is_file() {
                    // Device or other oddity
                    continue;
                }
                trace!("walkdir add_file {}", path.display());
                // It's either a file, or a symlink pointing to a file
                self.add_file(env_vars, path)?;
            }
            Ok(())
        }

        /// Simplify the path.
        /// Symlinks in the path are recorded for inclusion in the tarball.
        fn simplify_path<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
            SimplifyPath {
                dirs: Some(&mut self.dirs_set),
                resolved_symlinks: Some(&mut self.symlinks),
            }
            .simplify(path.as_ref())
        }

        pub fn build(self) -> Result<Arc<dyn super::PackagedToolchain>> {
            use itertools::Itertools;

            // Adjust the archive paths with the PathTransformer

            let dirs_set = self
                .dirs_set
                .into_iter()
                .map(|dir_path| {
                    self.path_transformer
                        .as_dist(&dir_path)
                        .with_context(|| format!("Unable to transform directory path {dir_path:?}"))
                        // Strip the leading slash
                        .map(tar_safe_path)
                        .map(|tar_path| (tar_path, dir_path))
                })
                .try_collect()
                .context("Failed transforming intermediate directory paths")?;

            let file_set = self
                .file_set
                .into_iter()
                .map(|src_path| {
                    self.path_transformer
                        .as_dist(&src_path)
                        .with_context(|| format!("Unable to transform file path {src_path:?}"))
                        // Strip the leading slash
                        .map(tar_safe_path)
                        .map(|tar_path| (tar_path, src_path))
                })
                .try_collect()
                .context("Failed transforming file paths")?;

            let symlinks = self
                .symlinks
                .into_iter()
                .map(|(src_path, dst_path)| {
                    self.path_transformer
                        .as_dist(&src_path)
                        .with_context(|| format!("Unable to transform symlink path {src_path:?}"))
                        // Strip the leading slash
                        .map(tar_safe_path)
                        .map(|tar_path| {
                            (
                                tar_path,
                                // Leave `dst_path` as absolute, assuming the tar will
                                // be used in a chroot-like environment.
                                dst_path,
                            )
                        })
                })
                .try_collect()
                .context("Failed transforming symlink paths")?;

            Ok(Arc::new(PackagedToolchain {
                executable: self.executable,
                dirs_set,
                file_set,
                symlinks,
            }))
        }
    }

    pub struct PackagedToolchain {
        executable: PathBuf,
        // Put dirs and file in a deterministic order (map from tar_path -> real_path)
        dirs_set: BTreeMap<PathBuf, PathBuf>,
        file_set: BTreeMap<PathBuf, PathBuf>,
        // Symlinks to add to the tar
        // These are _not_ tar safe, and must be made so before being added to the tar (see
        // `tar_safe_path`).
        symlinks: BTreeMap<PathBuf, PathBuf>,
    }

    #[async_trait]
    impl super::PackagedToolchain for PackagedToolchain {
        async fn compute_hash(&self) -> Result<String> {
            let mut digest = crate::util::Digest::new();

            for (src_path, dst_path) in self.symlinks.iter() {
                if dst_path.is_file() {
                    digest = digest.with_file(dst_path).await?;
                }
                digest.update(&path_to_bytes(dst_path)?);
                digest.update(&path_to_bytes(src_path)?);
            }
            for (tar_path, dir_path) in self.dirs_set.iter() {
                digest.update(&path_to_bytes(tar_path)?);
                digest.update(&path_to_bytes(dir_path)?);
            }
            for (tar_path, src_path) in self.file_set.iter() {
                digest = digest.with_file(src_path).await?;
                digest.update(&path_to_bytes(tar_path)?);
                digest.update(&path_to_bytes(src_path)?);
            }

            Ok(digest.finish())
        }

        async fn write_tar_gz(
            &self,
            toolchain: &dist::Toolchain,
            writer: &mut (dyn Write + Send),
        ) -> Result<()> {
            use gzp::{
                deflate::Gzip,
                par::compress::{Compression, ParCompressBuilder},
            };

            let dirs_set = self.dirs_set.clone();
            let file_set = self.file_set.clone();
            let symlinks = self.symlinks.clone();

            debug!(
                "Compressing toolchain for {:?} -> {:?}",
                self.executable.display(),
                toolchain.archive_id,
            );

            tokio::task::block_in_place(move || {
                std::thread::scope(|scope| {
                    let compressor = ParCompressBuilder::<Gzip>::new()
                        .compression_level(Compression::default())
                        .num_threads(crate::util::num_cpus())?
                        .from_borrowed_writer(writer, scope);

                    let mut builder = tar::Builder::new(compressor);

                    // Add directories before symlinks to ensure the dirs exist
                    // before symlinks that may point at them are unpacked
                    for (tar_path, dir_path) in dirs_set.iter() {
                        builder.append_dir(tar_path, dir_path)?;
                    }
                    // Add symlinks before files to ensure the symlinks exist
                    // before files are unpacked to paths that may traverse them
                    for (src_path, dst_path) in symlinks.iter() {
                        let mut header = tar::Header::new_gnu();
                        header.set_size(0);
                        header.set_mtime(0);
                        header.set_entry_type(tar::EntryType::Symlink);
                        // Leave `to_path` as absolute, assuming the tar will
                        // be used in a chroot-like environment.
                        builder.append_link(&mut header, tar_safe_path(src_path), dst_path)?;
                    }
                    for (tar_path, file_path) in file_set.iter() {
                        builder.append_path_with_name(file_path, tar_path)?;
                    }

                    builder.finish().map_err(anyhow::Error::new)
                })
            })
        }
    }

    // The dynamic linker is the only thing that truly knows how dynamic libraries will be
    // searched for, so we need to ask it directly.
    //
    // This function will extract any absolute paths from output like the following:
    // $ ldd /bin/ls
    //         linux-vdso.so.1 =>  (0x00007ffeb41f6000)
    //         libselinux.so.1 => /lib/x86_64-linux-gnu/libselinux.so.1 (0x00007f6877f4f000)
    //         libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f6877b85000)
    //         libpcre.so.3 => /lib/x86_64-linux-gnu/libpcre.so.3 (0x00007f6877915000)
    //         libdl.so.2 => /lib/x86_64-linux-gnu/libdl.so.2 (0x00007f6877711000)
    //         /lib64/ld-linux-x86-64.so.2 (0x00007f6878171000)
    //         libpthread.so.0 => /lib/x86_64-linux-gnu/libpthread.so.0 (0x00007f68774f4000)
    //
    // Elf executables can be statically or dynamically linked, and position independent (PIE) or not:
    // - dynamic + PIE = ET_DYN, ldd stdouts something like the list above and exits with code 0
    // - dynamic + non-PIE = ET_EXEC, ldd stdouts something like the list above and exits with code 0
    // - static + PIE = ET_DYN, ldd stdouts something like "\tstatically linked" or
    //   "\tldd (0x7f79ef662000)" and exits with code 0
    // - static + non-PIE = ET_EXEC, ldd stderrs something like "\tnot a dynamic executable" or
    //   "ldd: a.out: Not a valid dynamic program" and exits with code 1
    //
    #[cfg(not(windows))]
    fn find_ldd_libraries(
        env_vars: &[(OsString, OsString)],
        executable: &Path,
    ) -> Result<Vec<PathBuf>> {
        use std::io::Read;

        let process::Output {
            status,
            stdout,
            stderr,
        } = process::Command::new("ldd")
            .envs(env_vars.to_vec())
            .arg(executable)
            .output()?;

        // Not a file ldd can handle. This can be a non-executable, or a static non-PIE
        if !status.success() {
            // Best-effort detection of static non-PIE
            let mut elf = fs::File::open(executable)?;
            let mut elf_bytes = [0; 0x12];
            elf.read_exact(&mut elf_bytes)?;
            if elf_bytes[..0x4] != [0x7f, 0x45, 0x4c, 0x46] {
                bail!("Elf magic not found")
            }
            let little_endian = match elf_bytes[0x5] {
                1 => true,
                2 => false,
                _ => bail!("Invalid endianness in elf header"),
            };
            let e_type = if little_endian {
                ((elf_bytes[0x11] as u16) << 8) | elf_bytes[0x10] as u16
            } else {
                ((elf_bytes[0x10] as u16) << 8) | elf_bytes[0x11] as u16
            };
            if e_type != 0x02 {
                bail!("ldd failed on a non-ET_EXEC elf")
            }
            // It appears to be an ET_EXEC, good enough for us
            return Ok(vec![]);
        }

        if !stderr.is_empty() {
            trace!("ldd stderr non-empty: {:?}", bytes_to_string(stderr));
        }

        let stdout = bytes_to_string(stdout).context("ldd output not utf8")?;
        Ok(parse_ldd_output(&stdout))
    }

    // If it's a static PIE the output will be a line like "\tstatically linked", so be forgiving
    // in the parsing here and treat parsing oddities as an empty list.
    #[cfg(not(windows))]
    fn parse_ldd_output(stdout: &str) -> Vec<PathBuf> {
        let mut libs = vec![];
        for line in stdout.lines() {
            let line = line.trim();
            let mut parts: Vec<_> = line.split_whitespace().collect();

            // Remove a possible "(0xdeadbeef)" or assume this isn't a library line
            match parts.pop() {
                Some(s) if s.starts_with('(') && s.ends_with(')') => (),
                Some(_) | None => continue,
            }

            if parts.len() > 3 {
                continue;
            }

            let libpath = match (parts.first(), parts.get(1), parts.get(2)) {
                // "linux-vdso.so.1 =>  (0x00007ffeb41f6000)"
                (Some(_libname), Some(&"=>"), None) => continue,
                // "libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f6877b85000)"
                (Some(libname), Some(&"=>"), Some(libpath)) => {
                    // ldd (version 2.30) will output something like this:
                    //   ...
                    //   /lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2
                    //   ...
                    // We need to add /lib64/ld-linux-x86-64.so.2 to deps, else we'll get error "No
                    // such file or directory".
                    //
                    // Workaround: add libname to deps if it's absolute and exists.
                    let libname_path = PathBuf::from(libname);
                    if libname_path.is_absolute() {
                        libs.push(libname_path);
                    }

                    PathBuf::from(libpath)
                }
                // "/lib64/ld-linux-x86-64.so.2 (0x00007f6878171000)"
                (Some(libpath), None, None) => PathBuf::from(libpath),
                _ => continue,
            };

            if !libpath.is_absolute() {
                continue;
            }

            libs.push(libpath);
        }

        libs
    }

    #[cfg(windows)]
    fn find_ldd_libraries(
        env_vars: &[(OsString, OsString)],
        executable: &Path,
    ) -> Result<Vec<PathBuf>> {
        use crate::util::OsStrExt;

        let mut paths = vec![];

        let env_path = env_vars
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var_os("PATH"));

        let dumpbin = if let Some(dir) = executable.parent() {
            paths.push(dir.to_path_buf());
            which::which_in("dumpbin", env_path.as_ref(), dir)
        } else {
            which::which_in("dumpbin", env_path.as_ref(), ".")
        }
        .unwrap_or_else(|_| PathBuf::from("dumpbin"));

        let process::Output {
            status,
            stdout,
            stderr,
        } = process::Command::new(dumpbin)
            .envs(env_vars.to_vec())
            .arg("/dependents")
            .arg(executable)
            .output()?;

        if !stderr.is_empty() {
            trace!("dumpbin stderr: {:?}", bytes_to_string(stderr));
        }

        if !status.success() {
            return Ok(vec![]);
        }

        paths.extend(
            env_path
                .map(|ps| {
                    std::env::split_paths(&ps)
                        .filter(|p| p.is_dir())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );

        // Skip OS system directories, do not package core Windows files
        paths.retain(|p| {
            let p = p.as_os_str();
            !(
                p.contains(r"\Windows")
                    || p.contains(r"\windows")
                    || p.contains(r"\WINDOWS")
                    || p.contains(r"\System32")
                    || p.contains(r"\system32")
                    || p.contains(r"\SYSTEM32")
                //
            )
        });

        Ok(parse_ldd_output(&bytes_to_string(stdout)?)
            .iter()
            .filter(|lib| {
                // # Skip virtual API sets entirely
                !(lib.starts_with("api-ms-win-") || lib.starts_with("ext-ms-win-"))
            })
            .filter_map(|lib| {
                for dir in paths.iter() {
                    let lib = dir.join(lib);
                    if lib.exists() {
                        return Some(lib);
                    }
                }
                None
            })
            .collect::<Vec<_>>())
    }

    #[cfg(windows)]
    fn parse_ldd_output(stdout: &str) -> Vec<PathBuf> {
        stdout
            .lines()
            .map(|line| line.trim())
            .filter(|line| line.ends_with(std::env::consts::DLL_SUFFIX))
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    }

    #[test]
    #[cfg(not(windows))]
    fn test_ldd_parse() {
        let ubuntu_ls_output = "\tlinux-vdso.so.1 =>  (0x00007fffcfffe000)
\tlibselinux.so.1 => /lib/x86_64-linux-gnu/libselinux.so.1 (0x00007f69caa6b000)
\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f69ca6a1000)
\tlibpcre.so.3 => /lib/x86_64-linux-gnu/libpcre.so.3 (0x00007f69ca431000)
\tlibdl.so.2 => /lib/x86_64-linux-gnu/libdl.so.2 (0x00007f69ca22d000)
\t/lib64/ld-linux-x86-64.so.2 (0x00007f69cac8d000)
\tlibpthread.so.0 => /lib/x86_64-linux-gnu/libpthread.so.0 (0x00007f69ca010000)
";
        assert_eq!(
            parse_ldd_output(ubuntu_ls_output)
                .iter()
                .map(|p| p.to_str().unwrap())
                .collect::<Vec<_>>(),
            &[
                "/lib/x86_64-linux-gnu/libselinux.so.1",
                "/lib/x86_64-linux-gnu/libc.so.6",
                "/lib/x86_64-linux-gnu/libpcre.so.3",
                "/lib/x86_64-linux-gnu/libdl.so.2",
                "/lib64/ld-linux-x86-64.so.2",
                "/lib/x86_64-linux-gnu/libpthread.so.0",
            ]
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn test_ldd_parse_static() {
        let static_outputs = &[
            "\tstatically linked",    // glibc ldd output
            "\tldd (0x7f79ef662000)", // musl ldd output
        ];
        for static_output in static_outputs {
            assert_eq!(parse_ldd_output(static_output).len(), 0);
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn test_ldd_parse_v2_30() {
        let archlinux_ls_output = "\tlinux-vdso.so.1 (0x00007ffddc1f6000)
\tlibcap.so.2 => /usr/lib/libcap.so.2 (0x00007f4980989000)
\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f69ca6a1000)
\tlibc.so.6 => /usr/lib/libc.so.6 (0x00007f49807c2000)
\t/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x00007f49809e9000)
";
        assert_eq!(
            parse_ldd_output(archlinux_ls_output)
                .iter()
                .map(|p| p.to_str().unwrap())
                .collect::<Vec<_>>(),
            &[
                "/usr/lib/libcap.so.2",
                "/lib/x86_64-linux-gnu/libc.so.6",
                "/usr/lib/libc.so.6",
                "/lib64/ld-linux-x86-64.so.2",
                "/usr/lib64/ld-linux-x86-64.so.2",
            ]
        );
    }
}

/// Strip a leading slash, if any.
pub fn tar_safe_path<P: AsRef<Path>>(path: P) -> PathBuf {
    let mut final_path = PathBuf::new();

    for component in path.as_ref().components() {
        match component {
            Component::RootDir | Component::Prefix(_) => continue,
            c @ Component::Normal(_) | c @ Component::CurDir | c @ Component::ParentDir => {
                final_path.push(c);
            }
        }
    }

    final_path
}

pub fn make_tar_header<P: AsRef<Path>>(src: P, dst: P) -> io::Result<(tar::Header, PathBuf)> {
    let src = src.as_ref();
    let metadata_res = fs::metadata(src);

    let mut file_header = tar::Header::new_ustar();
    // TODO: test this works
    if let Ok(metadata) = metadata_res {
        // TODO: if the source file is a symlink, I think this does bad things
        file_header.set_metadata(&metadata);
    } else {
        warn!("Couldn't get metadata of file {src:?}, falling back to some defaults");
        file_header.set_mode(0o644);
        file_header.set_uid(0);
        file_header.set_gid(0);
        file_header.set_mtime(0);
        file_header
            .set_device_major(0)
            .expect("expected a ustar header");
        file_header
            .set_device_minor(0)
            .expect("expected a ustar header");
        file_header.set_entry_type(tar::EntryType::file());
    }

    Ok((file_header, tar_safe_path(dst)))
}

/// Simplify a path to one without any relative components, erroring if it looks
/// like there could be any symlink complexity that means a simplified path is not
/// equivalent to the original (see the documentation of `fs::canonicalize` for an
/// example).
///
/// So why avoid resolving symlinks? Any path that we are trying to simplify has
/// (usually) been added to an archive because something will try access it, but
/// resolving symlinks (be they for the actual file or directory components) can
/// make the accessed path 'disappear' in favour of the canonical path.
pub fn simplify_path<P: AsRef<Path>>(path: P) -> Result<PathBuf> {
    SimplifyPath {
        dirs: None,
        resolved_symlinks: None,
    }
    .simplify(path.as_ref())
}

pub struct SimplifyPath<'a> {
    pub dirs: Option<&'a mut std::collections::BTreeSet<PathBuf>>,
    pub resolved_symlinks: Option<&'a mut std::collections::BTreeMap<PathBuf, PathBuf>>,
}

impl SimplifyPath<'_> {
    pub fn simplify<P: AsRef<Path>>(&mut self, path: P) -> Result<PathBuf> {
        let mut final_path = PathBuf::new();
        for component in path.as_ref().components() {
            match component {
                c @ Component::RootDir | c @ Component::Prefix(_) | c @ Component::Normal(_) => {
                    final_path.push(c);
                    if self.resolved_symlinks.is_some() && final_path.is_symlink() {
                        let parent = final_path.parent().expect("symlinks have parents");
                        let link_target = final_path.read_link()?;
                        let new_final_path = self.simplify(parent.join(&link_target))?;
                        let old_final_path =
                            std::mem::replace(&mut final_path, new_final_path.clone());
                        #[allow(clippy::unnecessary_unwrap)]
                        self.resolved_symlinks
                            .as_mut()
                            .unwrap()
                            .insert(old_final_path, new_final_path);
                    }
                }
                Component::ParentDir => {
                    // If the path is doing funny symlink traversals, just give up.
                    //
                    // This case should only occur if `resolved_symlinks` is `None`.
                    if final_path.is_symlink() {
                        bail!("Cannot handle symlinks in parent paths")
                    }
                    if let Some(dirs) = self.dirs.as_mut() {
                        dirs.insert(final_path.clone());
                    }
                    final_path.pop();
                }
                Component::CurDir => continue,
            }
        }
        Ok(final_path)
    }
}
