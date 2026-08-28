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

//! Checked-in prost/tonic bindings for the Bazel Remote Execution v2 API.
//!
//! These are generated from the vendored `.proto` files in `proto/` by
//! `scripts/gen-reapi-proto`. They are checked in rather than built by a
//! build script so that building sccache needs neither `protoc` nor
//! `prost-build`/`tonic-build`. Re-generate with:
//!
//! ```text
//! cd scripts/gen-reapi-proto && cargo run -- ../..
//! ```
//!
//! The module nesting below must mirror the protobuf package names exactly,
//! because the generated code refers to sibling packages by relative `super::`
//! paths.

#![allow(
    clippy::all,
    clippy::pedantic,
    rustdoc::all,
    unreachable_pub,
    unused_qualifications
)]
#![cfg_attr(rustfmt, rustfmt::skip)]

pub mod build {
    pub mod bazel {
        pub mod semver {
            include!("build.bazel.semver.rs");
        }
        pub mod remote {
            pub mod execution {
                pub mod v2 {
                    include!("build.bazel.remote.execution.v2.rs");
                }
            }
        }
    }
}

pub mod google {
    pub mod bytestream {
        include!("google.bytestream.rs");
    }
    pub mod longrunning {
        include!("google.longrunning.rs");
    }
    pub mod rpc {
        include!("google.rpc.rs");
    }
}
