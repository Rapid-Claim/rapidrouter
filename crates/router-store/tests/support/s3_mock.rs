//! Enough of S3 to exercise the real SDK: GET, conditional PUT, list,
//! delete. The point is that the backend's conditional-write logic is
//! tested over the wire the SDK actually speaks, rather than against a
//! hand-rolled trait double that would agree with whatever we assumed.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(Clone)]
struct Object {
    body: Bytes,
    etag: String,
    modified_ms: u64,
}

#[derive(Default)]
pub struct Bucket {
    objects: BTreeMap<String, Object>,
    next_etag: u64,
    /// Set to make every subsequent request fail, to test that an outage
    /// stops writes without stopping reads from cache.
    pub offline: bool,
}

#[derive(Clone, Default)]
pub struct S3Mock {
    inner: Arc<Mutex<Bucket>>,
}

impl S3Mock {
    /// Bind on a random port and serve until the process exits.
    pub async fn spawn() -> (Self, String) {
        let mock = Self::default();
        let app = axum::Router::new()
            // The SDK lists with a trailing slash and gets objects without
            // one, so both spellings of "the bucket itself" are routed.
            .route("/{bucket}", get(list_objects))
            .route("/{bucket}/", get(list_objects))
            .route(
                "/{bucket}/{*key}",
                get(get_object).put(put_object).delete(delete_object),
            )
            .fallback(unexpected)
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (mock, format!("http://{addr}"))
    }

    pub fn set_offline(&self, offline: bool) {
        self.inner.lock().unwrap().offline = offline;
    }

    /// Overwrite an object behind the backend's back, simulating another
    /// node's commit landing between our read and our write.
    pub fn force_put(&self, key: &str, body: &[u8]) {
        let mut bucket = self.inner.lock().unwrap();
        bucket.next_etag += 1;
        let etag = format!("\"{}\"", bucket.next_etag);
        bucket.objects.insert(
            key.to_owned(),
            Object {
                body: Bytes::copy_from_slice(body),
                etag,
                modified_ms: now_ms(),
            },
        );
    }

    pub fn object_count(&self, prefix: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .objects
            .keys()
            .filter(|k| k.starts_with(prefix))
            .count()
    }

    /// Backdate an object so it falls outside a liveness window.
    pub fn age_object(&self, key: &str, by_ms: u64) {
        if let Some(object) = self.inner.lock().unwrap().objects.get_mut(key) {
            object.modified_ms = object.modified_ms.saturating_sub(by_ms);
        }
    }

    pub fn keys(&self) -> Vec<String> {
        self.inner.lock().unwrap().objects.keys().cloned().collect()
    }
}

/// Anything the backend asks for that this mock does not implement is a
/// request we did not mean to make; fail loudly rather than quietly.
async fn unexpected(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    panic!("the backend made an unexpected S3 request: {method} {uri}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn offline_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "<Error><Code>SlowDown</Code></Error>",
    )
        .into_response()
}

async fn get_object(
    State(mock): State<S3Mock>,
    Path((_bucket, key)): Path<(String, String)>,
) -> Response {
    let bucket = mock.inner.lock().unwrap();
    if bucket.offline {
        return offline_response();
    }
    match bucket.objects.get(&key) {
        Some(object) => (
            StatusCode::OK,
            [("ETag", object.etag.clone())],
            object.body.clone(),
        )
            .into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "<Error><Code>NoSuchKey</Code></Error>",
        )
            .into_response(),
    }
}

async fn put_object(
    State(mock): State<S3Mock>,
    Path((_bucket, key)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let mut bucket = mock.inner.lock().unwrap();
    if bucket.offline {
        return offline_response();
    }
    let existing = bucket.objects.get(&key).cloned();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    // The two preconditions the backend relies on. Anything else is a
    // request we did not intend to make, so fail loudly.
    if let Some(expected) = header("if-match") {
        match &existing {
            Some(object) if object.etag == expected => {}
            _ => return precondition_failed(),
        }
    }
    if let Some(value) = header("if-none-match") {
        assert_eq!(value, "*", "the backend only ever uses If-None-Match: *");
        if existing.is_some() {
            return precondition_failed();
        }
    }

    bucket.next_etag += 1;
    let etag = format!("\"{}\"", bucket.next_etag);
    bucket.objects.insert(
        key,
        Object {
            body,
            etag: etag.clone(),
            modified_ms: now_ms(),
        },
    );
    (StatusCode::OK, [("ETag", etag)], "").into_response()
}

fn precondition_failed() -> Response {
    (
        StatusCode::PRECONDITION_FAILED,
        "<Error><Code>PreconditionFailed</Code></Error>",
    )
        .into_response()
}

async fn delete_object(
    State(mock): State<S3Mock>,
    Path((_bucket, key)): Path<(String, String)>,
) -> Response {
    let mut bucket = mock.inner.lock().unwrap();
    if bucket.offline {
        return offline_response();
    }
    bucket.objects.remove(&key);
    StatusCode::NO_CONTENT.into_response()
}

async fn list_objects(
    State(mock): State<S3Mock>,
    Path(bucket_name): Path<String>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Response {
    let bucket = mock.inner.lock().unwrap();
    if bucket.offline {
        return offline_response();
    }
    let prefix = params.get("prefix").cloned().unwrap_or_default();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    xml.push_str(&format!("<Name>{bucket_name}</Name>"));
    xml.push_str("<IsTruncated>false</IsTruncated>");
    let mut count = 0;
    for (key, object) in bucket
        .objects
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
    {
        count += 1;
        let modified = aws_smithy_types::DateTime::from_millis(object.modified_ms as i64)
            .fmt(aws_smithy_types::date_time::Format::DateTime)
            .unwrap();
        xml.push_str(&format!(
            "<Contents><Key>{key}</Key><LastModified>{modified}</LastModified>\
             <ETag>{}</ETag><Size>{}</Size></Contents>",
            object.etag.replace('"', "&quot;"),
            object.body.len()
        ));
    }
    xml.push_str(&format!("<KeyCount>{count}</KeyCount></ListBucketResult>"));
    (StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response()
}
