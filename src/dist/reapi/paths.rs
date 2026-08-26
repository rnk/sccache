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

//! Translating between sccache's absolute dist paths and REv2's
//! input-root-relative ones.
//!
//! This is the entire impedance mismatch between the two systems. sccache-dist
//! unpacks a job into an overlay filesystem rooted at `/`, so absolute paths
//! keep working. An REv2 worker instead materializes an input root in some
//! directory of its choosing and does not chroot into it, so *every* path that
//! names something we shipped has to be relative or it will escape the input
//! root and resolve against the worker's own filesystem.

/// Strip the leading `/` that makes a dist path absolute, yielding a path
/// relative to the REv2 input root.
///
/// On Linux, dist paths are just local absolute paths, and the tar entries the
/// packagers produce have already had their leading separator removed by
/// [`crate::dist::pkg::tar_safe_path`], so this is the one and only conversion
/// between the two worlds.
pub fn strip_root(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

/// Express an input-root-relative path relative to `base`.
pub fn relativize(path: &str, base: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let base: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();

    let common = parts
        .iter()
        .zip(base.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut out: Vec<&str> = vec![".."; base.len() - common];
    out.extend_from_slice(&parts[common..]);

    if out.is_empty() {
        ".".to_owned()
    } else {
        out.join("/")
    }
}

/// The directory part of a root-relative path, or `""` for a path at the root.
pub fn parent_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn relativize_walks_up_and_down() {
        let base = "work/proj/main/a/build";
        assert_eq!(
            relativize("work/proj/main/a/build/lib/Support/x.o", base),
            "lib/Support/x.o"
        );
        assert_eq!(
            relativize("work/proj/main/a/src/lib/Support/x.cpp", base),
            "../src/lib/Support/x.cpp"
        );
        assert_eq!(
            relativize("work/toolchain/bin/clang++", base),
            "../../../../toolchain/bin/clang++"
        );
        assert_eq!(
            relativize("usr/bin/gcc", base),
            "../../../../../usr/bin/gcc"
        );
        assert_eq!(relativize(base, base), ".");
    }

    #[test]
    fn relativize_against_the_input_root() {
        assert_eq!(relativize("work/a/b.o", ""), "work/a/b.o");
    }

    #[test]
    fn strip_root_is_the_only_absolute_to_relative_conversion() {
        assert_eq!(strip_root("/work/x"), "work/x");
        assert_eq!(strip_root("work/x"), "work/x");
    }

    #[test]
    fn parents() {
        assert_eq!(
            parent_of("work/toolchain/bin/clang++"),
            "work/toolchain/bin"
        );
        assert_eq!(parent_of("lib64"), "");
    }
}
