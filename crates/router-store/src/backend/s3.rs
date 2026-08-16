//! S3 as the control plane.
//!
//! The document is one object, `<prefix>store.json`, and concurrency
//! comes from S3's own conditional writes: `If-None-Match: *` to create
//! it, `If-Match: <etag>` to replace it. Two nodes racing produce a 412
//! for the loser, which is exactly the conflict the admin API already
//! knows how to report.
//!
//! Heartbeats are objects under `<prefix>nodes/`. Their timestamps come
//! from S3's own `LastModified` rather than anything written inside them,
//! so counting the fleet is a single `ListObjectsV2` — no per-node GET.
//! The advertised address rides in the key after the node id, because it
//! is the only way to read it back out of a listing.

use std::time::Duration;

use aws_sdk_s3::Client;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;

use super::{ControlPlane, ControlPlaneError, Document, NodeBeat, Snapshot, live_within, now_ms};
use crate::state::StoreState;

pub struct S3Store {
    client: Client,
    bucket: String,
    prefix: String,
}

impl S3Store {
    pub async fn new(
        bucket: String,
        prefix: String,
        region: Option<String>,
        endpoint: Option<String>,
    ) -> Result<Self, ControlPlaneError> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .http_client(super::https_client());
        if let Some(region) = region {
            loader = loader.region(Region::new(region));
        }
        let shared = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&shared);
        builder = builder.http_client(super::https_client());
        if let Some(endpoint) = endpoint {
            // A custom endpoint means a test double or a private gateway,
            // neither of which does virtual-host-style buckets.
            builder = builder.endpoint_url(endpoint).force_path_style(true);
        }
        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket,
            prefix: normalize_prefix(&prefix),
        })
    }

    fn document_key(&self) -> String {
        format!("{}store.json", self.prefix)
    }

    fn nodes_prefix(&self) -> String {
        format!("{}nodes/", self.prefix)
    }
}

/// `""` and `"rapid"` and `"caret/"` should all mean the same place.
fn normalize_prefix(raw: &str) -> String {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

fn status_of<E>(err: &SdkError<E, aws_smithy_runtime_api::http::Response>) -> Option<u16> {
    match err {
        SdkError::ServiceError(e) => Some(e.raw().status().as_u16()),
        _ => None,
    }
}

#[async_trait::async_trait]
impl ControlPlane for S3Store {
    fn describe(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.document_key())
    }

    async fn load(&self) -> Result<Snapshot, ControlPlaneError> {
        let output = match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.document_key())
            .send()
            .await
        {
            Ok(output) => output,
            Err(err) if status_of(&err) == Some(404) => return Ok(Snapshot::empty()),
            Err(err) => return Err(ControlPlaneError::unavailable("reading from S3", err)),
        };
        let etag = output.e_tag().map(str::to_owned);
        let bytes = output
            .body
            .collect()
            .await
            .map_err(|e| ControlPlaneError::unavailable("reading the S3 object body", e))?
            .into_bytes();
        let (state, version) = Document::decode(&bytes)?;
        Ok(Snapshot {
            state,
            version,
            token: etag,
        })
    }

    async fn commit(
        &self,
        base: &Snapshot,
        next: StoreState,
    ) -> Result<Snapshot, ControlPlaneError> {
        let version = base.version + 1;
        let bytes = Document::encode(version, &next)?;
        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.document_key())
            .content_type("application/json")
            .body(ByteStream::from(bytes));
        request = match &base.token {
            Some(etag) => request.if_match(etag),
            // No token means we believe nothing is there. Say so, so that
            // two nodes bootstrapping at once cannot both "create" it.
            None => request.if_none_match("*"),
        };

        match request.send().await {
            Ok(output) => Ok(Snapshot {
                state: next,
                version,
                token: output.e_tag().map(str::to_owned),
            }),
            // 412 is a lost race on If-Match; 409 is S3 reporting a
            // conflicting concurrent write. Both mean re-read.
            Err(err) if matches!(status_of(&err), Some(412) | Some(409)) => {
                let actual = self.load().await.map(|s| s.version).unwrap_or(base.version);
                Err(ControlPlaneError::Conflict {
                    expected: base.version,
                    actual,
                })
            }
            Err(err) => Err(ControlPlaneError::unavailable("writing to S3", err)),
        }
    }

    async fn heartbeat(&self, beat: &NodeBeat) -> Result<(), ControlPlaneError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(beat_key(&self.nodes_prefix(), &beat.id, &beat.addr))
            .body(ByteStream::from_static(b"{}"))
            .send()
            .await
            .map_err(|e| ControlPlaneError::unavailable("writing a heartbeat to S3", e))?;
        Ok(())
    }

    async fn peers(&self, window: Duration) -> Result<Vec<NodeBeat>, ControlPlaneError> {
        let nodes_prefix = self.nodes_prefix();
        let mut beats = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(&nodes_prefix);
            if let Some(token) = continuation.take() {
                request = request.continuation_token(token);
            }
            let page = request
                .send()
                .await
                .map_err(|e| ControlPlaneError::unavailable("listing heartbeats in S3", e))?;
            for object in page.contents() {
                let Some(key) = object.key() else { continue };
                let Some((id, addr)) = parse_beat_key(&nodes_prefix, key) else {
                    continue;
                };
                let seen_ms = object
                    .last_modified()
                    .and_then(|t| t.to_millis().ok())
                    .unwrap_or(0)
                    .max(0) as u64;
                beats.push(NodeBeat { id, addr, seen_ms });
            }
            match page.next_continuation_token() {
                Some(token) => continuation = Some(token.to_owned()),
                None => break,
            }
        }
        Ok(live_within(beats, window, now_ms()))
    }

    async fn depart(&self, id: &str) -> Result<(), ControlPlaneError> {
        // The address is part of the key and we do not have it here, so
        // find the node's objects by prefix and drop them.
        let prefix = format!("{}{}.", self.nodes_prefix(), id);
        let page = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(&prefix)
            .send()
            .await
            .map_err(|e| ControlPlaneError::unavailable("listing this node's heartbeat", e))?;
        for object in page.contents() {
            if let Some(key) = object.key() {
                let _ = self
                    .client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .send()
                    .await;
            }
        }
        Ok(())
    }
}

/// `nodes/<id>.<base64url addr>` — one listing yields both fields.
fn beat_key(prefix: &str, id: &str, addr: &str) -> String {
    format!("{prefix}{id}.{}", B64.encode(addr.as_bytes()))
}

fn parse_beat_key(prefix: &str, key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix(prefix)?;
    let (id, encoded) = rest.split_once('.')?;
    let addr = B64
        .decode(encoded)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())?;
    Some((id.to_owned(), addr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_normalize_to_one_form() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("caret"), "caret/");
        assert_eq!(normalize_prefix("/caret/"), "caret/");
    }

    #[test]
    fn beat_keys_round_trip_including_awkward_addresses() {
        for addr in ["10.0.1.7:9443", "[::1]:9443", "host.internal:80"] {
            let key = beat_key("nodes/", "node-1", addr);
            assert_eq!(
                parse_beat_key("nodes/", &key),
                Some(("node-1".to_owned(), addr.to_owned()))
            );
        }
    }

    #[test]
    fn unrelated_keys_are_ignored_not_misparsed() {
        assert_eq!(parse_beat_key("nodes/", "other/thing"), None);
        assert_eq!(parse_beat_key("nodes/", "nodes/no-suffix"), None);
    }
}
