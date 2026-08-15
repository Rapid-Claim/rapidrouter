//! DynamoDB as the control plane.
//!
//! One table, two kinds of item, one composite key:
//!
//! ```text
//! pk = "store"  sk = "v1"      version (N), state (S, JSON)
//! pk = "nodes"  sk = <node id> addr (S), seen_ms (N), expires_at (N, TTL)
//! ```
//!
//! Putting every heartbeat under a single partition means counting the
//! fleet is one `Query` rather than a `Scan`, and enabling TTL on
//! `expires_at` means a node that dies without departing is swept up by
//! DynamoDB instead of by us. The document write is a `PutItem` with a
//! condition on the version, which is the same compare-and-swap the S3
//! backend gets from `If-Match`.

use std::collections::HashMap;
use std::time::Duration;

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::config::Region;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::put_item::PutItemError;
use aws_sdk_dynamodb::types::AttributeValue;

use super::{ControlPlane, ControlPlaneError, Document, NodeBeat, Snapshot, live_within, now_ms};
use crate::state::StoreState;

const STORE_PK: &str = "store";
const STORE_SK: &str = "v1";
const NODES_PK: &str = "nodes";

/// How long after its last heartbeat DynamoDB may delete a node's item.
/// Comfortably longer than any sane liveness window — TTL deletion is
/// best-effort and can lag by minutes, so it is a garbage collector, not
/// the liveness mechanism.
const BEAT_TTL: Duration = Duration::from_secs(3600);

pub struct DynamoStore {
    client: Client,
    table: String,
}

impl DynamoStore {
    pub async fn new(
        table: String,
        region: Option<String>,
        endpoint: Option<String>,
    ) -> Result<Self, ControlPlaneError> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .http_client(super::https_client());
        if let Some(region) = region {
            loader = loader.region(Region::new(region));
        }
        let shared = loader.load().await;
        let mut builder = aws_sdk_dynamodb::config::Builder::from(&shared);
        builder = builder.http_client(super::https_client());
        if let Some(endpoint) = endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        Ok(Self {
            client: Client::from_conf(builder.build()),
            table,
        })
    }

    fn key(pk: &str, sk: &str) -> HashMap<String, AttributeValue> {
        HashMap::from([
            ("pk".to_owned(), AttributeValue::S(pk.to_owned())),
            ("sk".to_owned(), AttributeValue::S(sk.to_owned())),
        ])
    }
}

fn is_condition_failure(
    err: &SdkError<PutItemError, aws_smithy_runtime_api::http::Response>,
) -> bool {
    matches!(err, SdkError::ServiceError(e) if e.err().is_conditional_check_failed_exception())
}

#[async_trait::async_trait]
impl ControlPlane for DynamoStore {
    fn describe(&self) -> String {
        format!("dynamodb://{}", self.table)
    }

    async fn load(&self) -> Result<Snapshot, ControlPlaneError> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table)
            .set_key(Some(Self::key(STORE_PK, STORE_SK)))
            // The whole point is reading our own writes. Eventually
            // consistent reads would let a node that just committed read
            // back the previous version and undo itself on the next edit.
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| ControlPlaneError::unavailable("reading from DynamoDB", e))?;

        let Some(item) = output.item else {
            return Ok(Snapshot::empty());
        };
        let body = match item.get("state") {
            Some(AttributeValue::S(json)) => json.as_bytes().to_vec(),
            _ => {
                return Err(ControlPlaneError::Fault(
                    "the store item has no `state` attribute; is another application using this table?".into(),
                ));
            }
        };
        let (state, version) = Document::decode(&body)?;
        Ok(Snapshot {
            state,
            version,
            token: Some(version.to_string()),
        })
    }

    async fn commit(
        &self,
        base: &Snapshot,
        next: StoreState,
    ) -> Result<Snapshot, ControlPlaneError> {
        let version = base.version + 1;
        let bytes = Document::encode(version, &next)?;
        let json = String::from_utf8(bytes)
            .map_err(|e| ControlPlaneError::fault("the encoded document is not UTF-8", e))?;

        let mut item = Self::key(STORE_PK, STORE_SK);
        item.insert("version".to_owned(), AttributeValue::N(version.to_string()));
        item.insert("state".to_owned(), AttributeValue::S(json));

        let request = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .condition_expression("attribute_not_exists(pk) OR version = :expected")
            .expression_attribute_values(":expected", AttributeValue::N(base.version.to_string()));

        match request.send().await {
            Ok(_) => Ok(Snapshot {
                state: next,
                version,
                token: Some(version.to_string()),
            }),
            Err(err) if is_condition_failure(&err) => {
                let actual = self.load().await.map(|s| s.version).unwrap_or(base.version);
                Err(ControlPlaneError::Conflict {
                    expected: base.version,
                    actual,
                })
            }
            Err(err) => Err(ControlPlaneError::unavailable("writing to DynamoDB", err)),
        }
    }

    async fn heartbeat(&self, beat: &NodeBeat) -> Result<(), ControlPlaneError> {
        let expires_at = (beat.seen_ms / 1000) + BEAT_TTL.as_secs();
        let mut item = Self::key(NODES_PK, &beat.id);
        item.insert("addr".to_owned(), AttributeValue::S(beat.addr.clone()));
        item.insert(
            "seen_ms".to_owned(),
            AttributeValue::N(beat.seen_ms.to_string()),
        );
        item.insert(
            "expires_at".to_owned(),
            AttributeValue::N(expires_at.to_string()),
        );
        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .send()
            .await
            .map_err(|e| ControlPlaneError::unavailable("writing a heartbeat to DynamoDB", e))?;
        Ok(())
    }

    async fn peers(&self, window: Duration) -> Result<Vec<NodeBeat>, ControlPlaneError> {
        let mut beats = Vec::new();
        let mut start: Option<HashMap<String, AttributeValue>> = None;
        loop {
            let output = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("pk = :pk")
                .expression_attribute_values(":pk", AttributeValue::S(NODES_PK.to_owned()))
                .set_exclusive_start_key(start.take())
                .send()
                .await
                .map_err(|e| ControlPlaneError::unavailable("listing heartbeats in DynamoDB", e))?;

            for item in output.items() {
                let Some(AttributeValue::S(id)) = item.get("sk") else {
                    continue;
                };
                let addr = match item.get("addr") {
                    Some(AttributeValue::S(addr)) => addr.clone(),
                    _ => String::new(),
                };
                let seen_ms = match item.get("seen_ms") {
                    Some(AttributeValue::N(n)) => n.parse().unwrap_or(0),
                    _ => 0,
                };
                beats.push(NodeBeat {
                    id: id.clone(),
                    addr,
                    seen_ms,
                });
            }

            match output.last_evaluated_key {
                Some(key) if !key.is_empty() => start = Some(key),
                _ => break,
            }
        }
        Ok(live_within(beats, window, now_ms()))
    }

    async fn depart(&self, id: &str) -> Result<(), ControlPlaneError> {
        self.client
            .delete_item()
            .table_name(&self.table)
            .set_key(Some(Self::key(NODES_PK, id)))
            .send()
            .await
            .map_err(|e| ControlPlaneError::unavailable("removing this node's heartbeat", e))?;
        Ok(())
    }
}
