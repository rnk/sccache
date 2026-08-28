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

//! Content-addressable storage: getting blobs to and from an REv2 server.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};

use tonic::transport::Channel;

use crate::errors::*;

use super::proto::{
    build::bazel::remote::execution::v2 as reapi,
    google::bytestream::{ReadRequest, WriteRequest, byte_stream_client::ByteStreamClient},
};
use super::{RpcContext, merkle::digest_bytes};

use reapi::content_addressable_storage_client::ContentAddressableStorageClient;

/// Chunk size for `ByteStream.Write`. gRPC's default maximum message size is
/// 4 MiB and the resource name rides along in the first message, so stay well
/// under it.
const STREAM_CHUNK_BYTES: usize = 1024 * 1024;

/// Fallback when the server does not advertise `max_batch_total_size_bytes`.
/// This is the value the REv2 spec suggests implementations assume.
const DEFAULT_MAX_BATCH_BYTES: i64 = 4 * 1024 * 1024;

/// Leave room in a batch request for the digests, instance name and framing
/// that accompany the blob payloads.
const BATCH_OVERHEAD_BYTES: i64 = 64 * 1024;

pub struct Cas {
    rpc: Arc<RpcContext>,
    cas: ContentAddressableStorageClient<Channel>,
    bytestream: ByteStreamClient<Channel>,
    max_batch_bytes: i64,
    /// Digests this client has already seen the server acknowledge.
    ///
    /// A build recompiles against the same toolchain and the same system
    /// headers thousands of times, so without this every action would re-ask
    /// about the same blobs. Bounded so a long-lived server process cannot
    /// grow it without limit.
    known_present: Mutex<HashSet<String>>,
}

/// Cap on `known_present`. Each entry is a 64-character hex digest, so this is
/// a few megabytes at most.
const MAX_KNOWN_PRESENT: usize = 50_000;

impl Cas {
    pub fn new(rpc: Arc<RpcContext>, channel: Channel, max_batch_bytes: i64) -> Self {
        let max_batch_bytes = if max_batch_bytes > 0 {
            max_batch_bytes
        } else {
            DEFAULT_MAX_BATCH_BYTES
        };
        Self {
            cas: ContentAddressableStorageClient::new(channel.clone())
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
            bytestream: ByteStreamClient::new(channel)
                .max_decoding_message_size(usize::MAX)
                .max_encoding_message_size(usize::MAX),
            rpc,
            max_batch_bytes,
            known_present: Default::default(),
        }
    }

    fn note_present(&self, hashes: impl IntoIterator<Item = String>) {
        let mut known = self.known_present.lock().unwrap();
        if known.len() >= MAX_KNOWN_PRESENT {
            known.clear();
        }
        known.extend(hashes);
    }

    fn is_known_present(&self, hash: &str) -> bool {
        self.known_present.lock().unwrap().contains(hash)
    }

    /// Forget everything we believe the server has.
    ///
    /// Must be called when the server reports that a blob an action referenced
    /// has gone missing. Without this, `missing()` would keep filtering out
    /// the very digests that need re-uploading, the resubmit would upload
    /// nothing, and the retry would fail identically forever.
    pub fn forget_known_present(&self) {
        self.known_present.lock().unwrap().clear();
    }

    /// Which of these blobs does the server not have?
    ///
    /// Digests already known to be present are filtered out client-side and
    /// never make it into a request.
    pub async fn missing(&self, digests: &[reapi::Digest]) -> Result<Vec<reapi::Digest>> {
        // Deduplicate: the same header or the same empty file can easily appear
        // many times in one input root.
        let mut seen = HashSet::new();
        let candidates: Vec<reapi::Digest> = digests
            .iter()
            .filter(|d| !self.is_known_present(&d.hash))
            .filter(|d| seen.insert(d.hash.clone()))
            .cloned()
            .collect();

        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        let mut missing = Vec::new();
        // Servers commonly cap the number of digests per call; 1000 is the
        // conventional chunk size and is well under any published limit.
        for chunk in candidates.chunks(1000) {
            let res = self
                .cas
                .clone()
                .find_missing_blobs(self.rpc.request(reapi::FindMissingBlobsRequest {
                    instance_name: self.rpc.instance_name.clone(),
                    blob_digests: chunk.to_vec(),
                    digest_function: reapi::digest_function::Value::Sha256 as i32,
                }))
                .await
                .map_err(|e| super::rpc_error(e, "FindMissingBlobs"))?
                .into_inner();

            let missing_here: HashSet<String> = res
                .missing_blob_digests
                .iter()
                .map(|d| d.hash.clone())
                .collect();

            self.note_present(
                chunk
                    .iter()
                    .map(|d| d.hash.clone())
                    .filter(|h| !missing_here.contains(h)),
            );

            missing.extend(res.missing_blob_digests);
        }

        Ok(missing)
    }

    /// Upload a set of blobs, packing them into as few `BatchUpdateBlobs`
    /// calls as the server's size limit allows and streaming anything too
    /// large to batch.
    pub async fn upload_all(&self, blobs: Vec<(reapi::Digest, Vec<u8>)>) -> Result<()> {
        let batch_limit = self.max_batch_bytes - BATCH_OVERHEAD_BYTES;
        let mut batch: Vec<(reapi::Digest, Vec<u8>)> = Vec::new();
        let mut batch_bytes = 0i64;

        for (digest, data) in blobs {
            if digest.size_bytes > batch_limit {
                self.upload_stream(digest, data).await?;
                continue;
            }
            if batch_bytes + digest.size_bytes > batch_limit && !batch.is_empty() {
                self.upload_batch(std::mem::take(&mut batch)).await?;
                batch_bytes = 0;
            }
            batch_bytes += digest.size_bytes;
            batch.push((digest, data));
        }

        if !batch.is_empty() {
            self.upload_batch(batch).await?;
        }

        Ok(())
    }

    async fn upload_batch(&self, blobs: Vec<(reapi::Digest, Vec<u8>)>) -> Result<()> {
        use reapi::batch_update_blobs_request::Request;

        if blobs.is_empty() {
            return Ok(());
        }

        let hashes: Vec<String> = blobs.iter().map(|(d, _)| d.hash.clone()).collect();
        let res = self
            .cas
            .clone()
            .batch_update_blobs(
                self.rpc.request(reapi::BatchUpdateBlobsRequest {
                    instance_name: self.rpc.instance_name.clone(),
                    requests: blobs
                        .into_iter()
                        .map(|(digest, data)| Request {
                            digest: Some(digest),
                            data,
                            compressor: reapi::compressor::Value::Identity as i32,
                        })
                        .collect(),
                    digest_function: reapi::digest_function::Value::Sha256 as i32,
                }),
            )
            .await
            .map_err(|e| super::rpc_error(e, "BatchUpdateBlobs"))?
            .into_inner();

        // A batch call succeeds at the RPC level even when individual blobs
        // failed, so the per-blob statuses have to be checked.
        for response in &res.responses {
            if let Some(status) = &response.status
                && status.code != 0
            {
                bail!(
                    "Failed to upload blob {}: {} (code {})",
                    response
                        .digest
                        .as_ref()
                        .map(|d| d.hash.as_str())
                        .unwrap_or("<unknown>"),
                    status.message,
                    status.code
                );
            }
        }

        self.note_present(hashes);
        Ok(())
    }

    async fn upload_stream(&self, digest: reapi::Digest, data: Vec<u8>) -> Result<()> {
        let resource_name = format!(
            "{}uploads/{}/blobs/{}/{}",
            instance_prefix(&self.rpc.instance_name),
            uuid::Uuid::new_v4(),
            digest.hash,
            digest.size_bytes
        );

        // Produce the chunks lazily. Materializing the whole `Vec<WriteRequest>`
        // up front would hold a second complete copy of the blob, which for a
        // 100 MB compiler is 100 MB of avoidable peak memory.
        let requests =
            futures::stream::unfold((data, 0usize, false), move |(data, offset, done)| {
                let resource_name = resource_name.clone();
                async move {
                    if done {
                        return None;
                    }
                    let end = (offset + STREAM_CHUNK_BYTES).min(data.len());
                    let finish_write = end == data.len();
                    let request = WriteRequest {
                        // Only the first request has to name the resource.
                        resource_name: if offset == 0 {
                            resource_name
                        } else {
                            String::new()
                        },
                        write_offset: offset as i64,
                        finish_write,
                        data: data[offset..end].to_vec(),
                    };
                    // A zero-length blob still needs one request to mark the
                    // write done, so `done` is what terminates the stream
                    // rather than the offset reaching the end.
                    Some((request, (data, end, finish_write)))
                }
            });

        let res = self
            .bytestream
            .clone()
            .write(self.rpc.request(requests))
            .await
            .map_err(|e| super::rpc_error(e, "ByteStream.Write"))?
            .into_inner();

        if res.committed_size != digest.size_bytes {
            bail!(
                "Server committed {} bytes of blob {} but expected {}",
                res.committed_size,
                digest.hash,
                digest.size_bytes
            );
        }

        self.note_present([digest.hash]);
        Ok(())
    }

    /// Fetch one blob, choosing the batch or streaming path by size.
    pub async fn download(&self, digest: &reapi::Digest) -> Result<Vec<u8>> {
        if digest.size_bytes == 0 {
            return Ok(Vec::new());
        }
        if digest.size_bytes <= self.max_batch_bytes - BATCH_OVERHEAD_BYTES {
            let mut blobs = self.download_batch(std::slice::from_ref(digest)).await?;
            blobs
                .pop()
                .map(|(_, data)| data)
                .with_context(|| format!("Server returned no data for blob {}", digest.hash))
        } else {
            self.download_stream(digest).await
        }
    }

    async fn download_batch(
        &self,
        digests: &[reapi::Digest],
    ) -> Result<Vec<(reapi::Digest, Vec<u8>)>> {
        let res = self
            .cas
            .clone()
            .batch_read_blobs(self.rpc.request(reapi::BatchReadBlobsRequest {
                instance_name: self.rpc.instance_name.clone(),
                digests: digests.to_vec(),
                acceptable_compressors: vec![reapi::compressor::Value::Identity as i32],
                digest_function: reapi::digest_function::Value::Sha256 as i32,
            }))
            .await
            .map_err(|e| super::rpc_error(e, "BatchReadBlobs"))?
            .into_inner();

        let mut blobs = Vec::with_capacity(res.responses.len());
        for response in res.responses {
            if let Some(status) = &response.status
                && status.code != 0
            {
                bail!(
                    "Failed to read blob {}: {} (code {})",
                    response
                        .digest
                        .as_ref()
                        .map(|d| d.hash.as_str())
                        .unwrap_or("<unknown>"),
                    status.message,
                    status.code
                );
            }
            let digest = response
                .digest
                .context("BatchReadBlobs response entry has no digest")?;
            verify(&digest, &response.data)?;
            blobs.push((digest, response.data));
        }

        Ok(blobs)
    }

    async fn download_stream(&self, digest: &reapi::Digest) -> Result<Vec<u8>> {
        use futures::StreamExt;

        let resource_name = format!(
            "{}blobs/{}/{}",
            instance_prefix(&self.rpc.instance_name),
            digest.hash,
            digest.size_bytes
        );

        let mut stream = self
            .bytestream
            .clone()
            .read(self.rpc.request(ReadRequest {
                resource_name,
                read_offset: 0,
                read_limit: 0,
            }))
            .await
            .map_err(|e| super::rpc_error(e, "ByteStream.Read"))?
            .into_inner();

        let mut data = Vec::with_capacity(digest.size_bytes as usize);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| super::rpc_error(e, "ByteStream.Read"))?;
            data.extend_from_slice(&chunk.data);
        }

        verify(digest, &data)?;
        Ok(data)
    }
}

/// An REv2 resource name embeds the instance name as a leading path segment,
/// and omits it entirely when the instance name is empty.
fn instance_prefix(instance_name: &str) -> String {
    if instance_name.is_empty() {
        String::new()
    } else {
        format!("{instance_name}/")
    }
}

/// Content addressing is only worth anything if it is checked. A server that
/// hands back the wrong bytes for a digest would otherwise silently poison the
/// local object cache.
fn verify(digest: &reapi::Digest, data: &[u8]) -> Result<()> {
    let actual = digest_bytes(data);
    if actual.hash != digest.hash || actual.size_bytes != digest.size_bytes {
        bail!(
            "CAS returned data for {}/{} that hashes to {}/{}",
            digest.hash,
            digest.size_bytes,
            actual.hash,
            actual.size_bytes
        );
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn resource_names_handle_the_empty_instance() {
        assert_eq!(instance_prefix(""), "");
        assert_eq!(instance_prefix("main"), "main/");
    }

    #[test]
    fn verify_rejects_mismatched_content() {
        let digest = digest_bytes(b"hello");
        assert!(verify(&digest, b"hello").is_ok());
        assert!(verify(&digest, b"goodbye").is_err());
    }
}
