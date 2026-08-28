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

//! Building an REv2 input root out of the tar stream sccache already produces.
//!
//! `InputsPackager::write_inputs` and `ToolchainPackager` both emit tar
//! archives whose entry paths have already been made root-relative by
//! [`crate::dist::pkg::tar_safe_path`]. A tar of root-relative paths, with
//! modes and symlinks, *is* an REv2 input root, so converting one to the other
//! is a direct structural translation and needs no cooperation from the
//! compiler frontends.

use std::{
    collections::{BTreeMap, HashSet},
    io::{self, Read},
};

use sha2::{Digest as _, Sha256};

use crate::errors::*;

use super::{
    paths::{parent_of, relativize, strip_root},
    proto::build::bazel::remote::execution::v2 as reapi,
};

/// Hash a blob and describe it the way REv2 does.
pub fn digest_bytes(data: &[u8]) -> reapi::Digest {
    reapi::Digest {
        hash: hex(&Sha256::digest(data)),
        size_bytes: data.len() as i64,
    }
}

/// Serialize a protobuf message and digest the encoded form.
///
/// REv2 digests are always over the *serialized* bytes, so the encoding and
/// the digest have to be produced together to stay consistent.
pub fn digest_message<M: prost::Message>(msg: &M) -> (reapi::Digest, Vec<u8>) {
    let bytes = msg.encode_to_vec();
    (digest_bytes(&bytes), bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// A writer that only hashes, so a large tar entry can be digested without
/// being held in memory.
struct HashingSink {
    hasher: Sha256,
    len: u64,
}

impl io::Write for HashingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        self.len += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    pub digest: reapi::Digest,
    pub is_executable: bool,
}

/// A mutable input root under construction.
///
/// `BTreeMap` is doing real work here: REv2 requires the `files`,
/// `directories` and `symlinks` of a `Directory` to each be sorted
/// lexicographically by name, and Rust's `String` ordering is byte-wise
/// lexicographic, which is exactly the required order. Getting this wrong
/// would not fail loudly -- it would silently produce a different action
/// digest than every other REv2 client, defeating the remote action cache.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DirBuilder {
    dirs: BTreeMap<String, DirBuilder>,
    files: BTreeMap<String, FileEntry>,
    symlinks: BTreeMap<String, String>,
}

/// Split a root-relative tar path into components, dropping `.` segments.
fn split_path(path: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => bail!("Refusing to build an input root from path with '..': {path:?}"),
            part => parts.push(part),
        }
    }
    Ok(parts)
}

impl DirBuilder {
    /// Walk to (creating as needed) the directory holding `path`, returning it
    /// and the final component's name.
    fn entry_for<'a>(&mut self, path: &'a str) -> Result<Option<(&mut DirBuilder, &'a str)>> {
        let parts = split_path(path)?;
        let Some((name, dirs)) = parts.split_last() else {
            // The archive named the root itself; nothing to insert.
            return Ok(None);
        };
        let mut node = self;
        for dir in dirs {
            node = node.dirs.entry((*dir).to_owned()).or_default();
        }
        Ok(Some((node, name)))
    }

    pub fn insert_dir(&mut self, path: &str) -> Result<()> {
        if let Some((parent, name)) = self.entry_for(path)? {
            parent.dirs.entry(name.to_owned()).or_default();
        }
        Ok(())
    }

    pub fn insert_file(
        &mut self,
        path: &str,
        digest: reapi::Digest,
        is_executable: bool,
    ) -> Result<()> {
        let Some((parent, name)) = self.entry_for(path)? else {
            bail!("Cannot insert a file at the input root itself: {path:?}");
        };
        parent.files.insert(
            name.to_owned(),
            FileEntry {
                digest,
                is_executable,
            },
        );
        Ok(())
    }

    /// Insert a symlink, rewriting an absolute target so it stays inside the
    /// input root.
    ///
    /// The packagers emit absolute targets -- `work/toolchain/bin/clang++ ->
    /// /work/toolchain/bin/clang` -- because sccache-dist unpacks a job into an
    /// overlay rooted at `/`, where that resolves correctly. An REv2 worker
    /// does not chroot, so an absolute target escapes the input root and
    /// dangles, and `fork/exec` of such a symlink fails with ENOENT. REv2
    /// servers also advertise a `symlink_absolute_path_strategy`, and
    /// commonly reject absolute targets outright.
    ///
    /// Every absolute path in these archives came from the same root, so
    /// re-anchoring the target relative to the symlink's own directory
    /// preserves what the packager meant.
    pub fn insert_symlink(&mut self, path: &str, target: &str) -> Result<()> {
        let target = if target.starts_with('/') {
            relativize(strip_root(target), parent_of(strip_root(path)))
        } else {
            target.to_owned()
        };
        let Some((parent, name)) = self.entry_for(path)? else {
            bail!("Cannot insert a symlink at the input root itself: {path:?}");
        };
        parent.symlinks.insert(name.to_owned(), target);
        Ok(())
    }

    /// Is there a file, directory or symlink at this root-relative path?
    ///
    /// Used to decide whether an absolute path in the compiler's argument list
    /// refers to something we are actually shipping, and therefore needs to be
    /// rewritten to be relative to the working directory.
    pub fn contains(&self, path: &str) -> bool {
        let Ok(parts) = split_path(path) else {
            return false;
        };
        let Some((name, dirs)) = parts.split_last() else {
            return true;
        };
        let mut node = self;
        for dir in dirs {
            match node.dirs.get(*dir) {
                Some(next) => node = next,
                None => return false,
            }
        }
        node.files.contains_key(*name)
            || node.symlinks.contains_key(*name)
            || node.dirs.contains_key(*name)
    }

    /// Overlay `other` onto `self`. Entries in `other` win on conflict.
    pub fn merge(&mut self, other: DirBuilder) {
        let DirBuilder {
            dirs,
            files,
            symlinks,
        } = other;
        for (name, dir) in dirs {
            self.dirs.entry(name).or_default().merge(dir);
        }
        self.files.extend(files);
        self.symlinks.extend(symlinks);
    }

    /// Encode the tree, returning the root digest along with every `Directory`
    /// blob that has to exist in the CAS for that digest to be resolvable.
    pub fn finish(&self) -> (reapi::Digest, Vec<(reapi::Digest, Vec<u8>)>) {
        let mut blobs = Vec::new();
        let root = self.encode(&mut blobs);
        (root, blobs)
    }

    fn encode(&self, blobs: &mut Vec<(reapi::Digest, Vec<u8>)>) -> reapi::Digest {
        let directories = self
            .dirs
            .iter()
            .map(|(name, dir)| reapi::DirectoryNode {
                name: name.clone(),
                digest: Some(dir.encode(blobs)),
            })
            .collect();

        let dir = reapi::Directory {
            files: self
                .files
                .iter()
                .map(|(name, entry)| reapi::FileNode {
                    name: name.clone(),
                    digest: Some(entry.digest.clone()),
                    is_executable: entry.is_executable,
                    node_properties: None,
                })
                .collect(),
            directories,
            symlinks: self
                .symlinks
                .iter()
                .map(|(name, target)| reapi::SymlinkNode {
                    name: name.clone(),
                    target: target.clone(),
                    node_properties: None,
                })
                .collect(),
            node_properties: None,
        };

        let (digest, bytes) = digest_message(&dir);
        blobs.push((digest.clone(), bytes));
        digest
    }
}

/// First pass over a tar archive: build the tree and collect the digests of
/// every file blob it references.
///
/// File contents are streamed through the hasher rather than buffered, so this
/// is safe to run over a toolchain archive containing a 100 MB compiler.
pub fn scan_tar<R: Read>(reader: R, tree: &mut DirBuilder) -> Result<Vec<reapi::Digest>> {
    use tar::EntryType;

    let mut archive = tar::Archive::new(reader);
    let mut digests = Vec::new();

    for entry in archive.entries().context("Failed to read inputs archive")? {
        let mut entry = entry.context("Failed to read inputs archive entry")?;
        let path = entry
            .path()
            .context("Inputs archive entry has an unrepresentable path")?
            .to_string_lossy()
            .into_owned();
        let mode = entry.header().mode().unwrap_or(0o644);

        match entry.header().entry_type() {
            EntryType::Directory => tree.insert_dir(&path)?,
            EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .context("Failed to read symlink target")?
                    .with_context(|| format!("Symlink {path:?} has no target"))?
                    .to_string_lossy()
                    .into_owned();
                tree.insert_symlink(&path, &target)?;
            }
            EntryType::Regular | EntryType::Continuous => {
                let mut sink = HashingSink {
                    hasher: Sha256::new(),
                    len: 0,
                };
                io::copy(&mut entry, &mut sink)
                    .with_context(|| format!("Failed to hash archive entry {path:?}"))?;
                let digest = reapi::Digest {
                    hash: hex(&sink.hasher.finalize()),
                    size_bytes: sink.len as i64,
                };
                digests.push(digest.clone());
                tree.insert_file(&path, digest, mode & 0o111 != 0)?;
            }
            // Hard links would need the CAS digest of their target, which the
            // packagers never emit; anything else is not representable in an
            // REv2 input root at all.
            other => bail!("Unsupported archive entry type {other:?} for {path:?}"),
        }
    }

    Ok(digests)
}

/// Second pass over the same tar archive: hand back the contents of just the
/// blobs the server told us it is missing.
///
/// A fully warm CAS returns immediately without touching the archive. Once
/// anything is missing, every entry whose size matches some wanted blob has to
/// be read to know its digest, but only one is held at a time and only genuine
/// matches are handed to the caller.
pub fn read_blobs<R, F>(
    reader: R,
    wanted: &HashSet<String>,
    wanted_sizes: &HashSet<u64>,
    mut sink: F,
) -> Result<()>
where
    R: Read,
    F: FnMut(reapi::Digest, Vec<u8>) -> Result<()>,
{
    use tar::EntryType;

    if wanted.is_empty() {
        return Ok(());
    }

    let mut archive = tar::Archive::new(reader);
    let mut seen = HashSet::new();

    for entry in archive
        .entries()
        .context("Failed to re-read inputs archive")?
    {
        let mut entry = entry.context("Failed to re-read inputs archive entry")?;
        if !matches!(
            entry.header().entry_type(),
            EntryType::Regular | EntryType::Continuous
        ) {
            continue;
        }

        // A blob can only match a wanted digest if its length matches too, so
        // size alone rules out most entries without reading them.
        let size = entry.header().size().unwrap_or(0);
        if !wanted_sizes.contains(&size) {
            continue;
        }

        let mut buf = Vec::with_capacity(size as usize);
        entry
            .read_to_end(&mut buf)
            .context("Failed to read archive entry contents")?;

        let digest = digest_bytes(&buf);
        if wanted.contains(&digest.hash) && seen.insert(digest.hash.clone()) {
            sink(digest, buf)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn tar_of(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, data, mode) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn digest_matches_known_sha256() {
        // `printf '' | sha256sum` and `printf 'hello' | sha256sum`.
        let empty = digest_bytes(b"");
        assert_eq!(
            empty.hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(empty.size_bytes, 0);

        let hello = digest_bytes(b"hello");
        assert_eq!(
            hello.hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(hello.size_bytes, 5);
    }

    #[test]
    fn empty_directory_has_the_canonical_digest() {
        // Every REv2 implementation agrees on the digest of an empty
        // `Directory`, because it serializes to zero bytes.
        let (digest, blobs) = DirBuilder::default().finish();
        assert_eq!(
            digest.hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(digest.size_bytes, 0);
        assert_eq!(blobs.len(), 1);
    }

    #[test]
    fn tar_becomes_a_tree() {
        let tar = tar_of(&[
            ("work/proj/src/a.cpp", b"int a;", 0o644),
            ("work/proj/build/gen.h", b"#define X", 0o644),
            ("usr/bin/clang", b"ELF", 0o755),
        ]);

        let mut tree = DirBuilder::default();
        let digests = scan_tar(&tar[..], &mut tree).unwrap();
        assert_eq!(digests.len(), 3);

        assert!(tree.contains("work/proj/src/a.cpp"));
        assert!(tree.contains("work/proj/build"));
        assert!(tree.contains("usr/bin/clang"));
        assert!(!tree.contains("work/proj/src/b.cpp"));
        assert!(!tree.contains("etc/passwd"));

        // Executable bits survive the round trip: this is what lets the worker
        // actually run a compiler shipped in the input root.
        let usr = &tree.dirs["usr"].dirs["bin"];
        assert!(usr.files["clang"].is_executable);
        assert!(!tree.dirs["work"].dirs["proj"].dirs["src"].files["a.cpp"].is_executable);

        // Every directory in the tree, including the root, is its own blob.
        let (_, blobs) = tree.finish();
        assert_eq!(blobs.len(), 7);
    }

    #[test]
    fn tree_encoding_is_order_independent() {
        // The action digest is only useful if it depends on tree *content* and
        // not on the order the archive happened to list entries in.
        let forward = tar_of(&[
            ("d/a.txt", b"a", 0o644),
            ("d/b.txt", b"b", 0o644),
            ("d/c.txt", b"c", 0o644),
        ]);
        let backward = tar_of(&[
            ("d/c.txt", b"c", 0o644),
            ("d/b.txt", b"b", 0o644),
            ("d/a.txt", b"a", 0o644),
        ]);

        let mut a = DirBuilder::default();
        scan_tar(&forward[..], &mut a).unwrap();
        let mut b = DirBuilder::default();
        scan_tar(&backward[..], &mut b).unwrap();

        assert_eq!(a.finish().0, b.finish().0);
    }

    #[test]
    fn merge_overlays_trees() {
        let inputs = tar_of(&[("work/src/a.cpp", b"int a;", 0o644)]);
        let toolchain = tar_of(&[("usr/bin/clang", b"ELF", 0o755)]);

        let mut tree = DirBuilder::default();
        scan_tar(&inputs[..], &mut tree).unwrap();
        let mut tc = DirBuilder::default();
        scan_tar(&toolchain[..], &mut tc).unwrap();
        tree.merge(tc);

        assert!(tree.contains("work/src/a.cpp"));
        assert!(tree.contains("usr/bin/clang"));

        // Merging is commutative for disjoint trees.
        let mut other = DirBuilder::default();
        scan_tar(&toolchain[..], &mut other).unwrap();
        let mut inputs_tree = DirBuilder::default();
        scan_tar(&inputs[..], &mut inputs_tree).unwrap();
        other.merge(inputs_tree);
        assert_eq!(tree.finish().0, other.finish().0);
    }

    #[test]
    fn read_blobs_returns_only_what_was_asked_for() {
        let tar = tar_of(&[
            ("a.txt", b"aaa", 0o644),
            ("b.txt", b"bbb", 0o644),
            ("c.txt", b"ccc", 0o644),
        ]);

        let wanted: HashSet<String> = [digest_bytes(b"bbb").hash].into_iter().collect();
        let wanted_sizes: HashSet<u64> = [3].into_iter().collect();
        let mut got = Vec::new();
        read_blobs(&tar[..], &wanted, &wanted_sizes, |digest, bytes| {
            got.push((digest.hash, bytes));
            Ok(())
        })
        .unwrap();

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"bbb");
    }

    #[test]
    fn absolute_symlink_targets_are_re_anchored() {
        let mut tree = DirBuilder::default();

        // The shape sccache's toolchain packager actually produces.
        tree.insert_symlink("work/toolchain/bin/clang++", "/work/toolchain/bin/clang")
            .unwrap();
        tree.insert_symlink(
            "usr/lib64/ld-linux-x86-64.so.2",
            "/usr/lib/x86_64-linux-gnu/ld.so",
        )
        .unwrap();
        tree.insert_symlink("lib64", "/usr/lib64").unwrap();
        // Relative targets are already correct and must be left alone.
        tree.insert_symlink("work/toolchain/bin/clang-cl", "clang")
            .unwrap();

        assert_eq!(
            tree.dirs["work"].dirs["toolchain"].dirs["bin"].symlinks["clang++"],
            "clang"
        );
        assert_eq!(
            tree.dirs["usr"].dirs["lib64"].symlinks["ld-linux-x86-64.so.2"],
            "../lib/x86_64-linux-gnu/ld.so"
        );
        assert_eq!(tree.symlinks["lib64"], "usr/lib64");
        assert_eq!(
            tree.dirs["work"].dirs["toolchain"].dirs["bin"].symlinks["clang-cl"],
            "clang"
        );
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let mut tree = DirBuilder::default();
        assert!(
            tree.insert_file("../escape", digest_bytes(b""), false)
                .is_err()
        );
    }
}
