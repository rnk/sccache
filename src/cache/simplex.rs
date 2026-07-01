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

use crate::{
    cache::{Cache, CacheMode, Storage},
    config::PreprocessorCacheModeConfig,
    errors::*,
};

use async_trait::async_trait;
use std::{sync::Arc, time::Duration};

pub struct SimplexCache(
    // reader cache
    pub Arc<dyn Storage>,
    // writer cache
    pub Arc<dyn Storage>,
);

impl SimplexCache {
    pub fn create(reader: Arc<dyn Storage>, writer: Arc<dyn Storage>) -> Arc<dyn Storage> {
        Arc::new(Self(reader, writer)) as Arc<dyn Storage>
    }
}

#[async_trait]
impl Storage for SimplexCache {
    async fn get(&self, key: &str) -> Result<Cache<opendal::Buffer>> {
        self.0.get(key).await
    }

    async fn del(&self, key: &str) -> Result<()> {
        self.1.del(key).await
    }

    async fn has(&self, key: &str) -> bool {
        self.0.has(key).await
    }

    async fn put(&self, key: &str, entry: opendal::Buffer) -> Result<Duration> {
        self.1.put(key, entry).await
    }

    async fn size(&self, key: &str) -> Result<u64> {
        self.0.size(key).await
    }

    /// Check the cache capability.
    async fn check(&self) -> Result<CacheMode> {
        self.1.check().await
    }

    /// Get the storage location.
    async fn location(&self) -> String {
        [self.0.location().await, self.1.location().await].join("\n")
    }

    /// Get the cache backend type name (e.g., "disk", "redis", "s3").
    /// Used for statistics and display purposes.
    fn cache_type_name(&self) -> &'static str {
        self.0.cache_type_name()
    }

    /// Get the current storage usage, if applicable.
    async fn current_size(&self) -> Result<Option<u64>> {
        self.0.current_size().await
    }

    /// Get the maximum storage size, if applicable.
    async fn max_size(&self) -> Result<Option<u64>> {
        self.0.max_size().await
    }

    /// Return the config for preprocessor cache mode if applicable
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        self.1.preprocessor_cache_mode_config()
    }

    fn basedirs(&self) -> &[Vec<u8>] {
        self.0.basedirs()
    }
}
