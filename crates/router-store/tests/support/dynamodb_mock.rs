//! Enough of DynamoDB to exercise the real SDK: GetItem, conditional
//! PutItem, Query, DeleteItem, over the JSON 1.0 protocol the SDK speaks.
//!
//! The condition expression is not interpreted in general — only the one
//! the backend emits is understood, and anything else panics. A mock that
//! quietly accepted an expression it did not evaluate would make the
//! compare-and-swap test pass without testing anything.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

type Item = BTreeMap<String, Value>;

#[derive(Default)]
struct Table {
    /// (pk, sk) -> item
    items: BTreeMap<(String, String), Item>,
    offline: bool,
}

#[derive(Clone, Default)]
pub struct DynamoMock {
    inner: Arc<Mutex<Table>>,
}

impl DynamoMock {
    pub async fn spawn() -> (Self, String) {
        let mock = Self::default();
        let app = axum::Router::new()
            .route("/", axum::routing::post(dispatch))
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

    /// Write behind the backend's back, as another node would.
    pub fn force_put(&self, pk: &str, sk: &str, item: Item) {
        self.inner
            .lock()
            .unwrap()
            .items
            .insert((pk.to_owned(), sk.to_owned()), item);
    }

    pub fn get(&self, pk: &str, sk: &str) -> Option<Item> {
        self.inner
            .lock()
            .unwrap()
            .items
            .get(&(pk.to_owned(), sk.to_owned()))
            .cloned()
    }

    pub fn count(&self, pk: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .items
            .keys()
            .filter(|(p, _)| p == pk)
            .count()
    }
}

fn string_attr(item: &Item, name: &str) -> Option<String> {
    item.get(name)?.get("S")?.as_str().map(str::to_owned)
}

fn number_attr(item: &Item, name: &str) -> Option<u64> {
    item.get(name)?.get("N")?.as_str()?.parse().ok()
}

async fn dispatch(State(mock): State<DynamoMock>, headers: HeaderMap, body: String) -> Response {
    if mock.inner.lock().unwrap().offline {
        return service_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "com.amazonaws.dynamodb.v20120810#InternalServerError",
            "the table is unavailable",
        );
    }
    let target = headers
        .get("x-amz-target")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_owned();
    let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

    match target.as_str() {
        "GetItem" => get_item(&mock, &request),
        "PutItem" => put_item(&mock, &request),
        "Query" => query(&mock, &request),
        "DeleteItem" => delete_item(&mock, &request),
        other => panic!("the backend made an unexpected DynamoDB call: {other}"),
    }
}

fn key_of(request: &Value, field: &str) -> (String, String) {
    let key = &request[field];
    (
        key["pk"]["S"].as_str().unwrap_or_default().to_owned(),
        key["sk"]["S"].as_str().unwrap_or_default().to_owned(),
    )
}

fn get_item(mock: &DynamoMock, request: &Value) -> Response {
    let (pk, sk) = key_of(request, "Key");
    let table = mock.inner.lock().unwrap();
    match table.items.get(&(pk, sk)) {
        Some(item) => axum::Json(json!({ "Item": item })).into_response(),
        None => axum::Json(json!({})).into_response(),
    }
}

fn put_item(mock: &DynamoMock, request: &Value) -> Response {
    let item: Item = serde_json::from_value(request["Item"].clone()).unwrap_or_default();
    let pk = string_attr(&item, "pk").unwrap_or_default();
    let sk = string_attr(&item, "sk").unwrap_or_default();
    let mut table = mock.inner.lock().unwrap();
    let existing = table.items.get(&(pk.clone(), sk.clone())).cloned();

    if let Some(expression) = request["ConditionExpression"].as_str() {
        assert_eq!(
            expression, "attribute_not_exists(pk) OR version = :expected",
            "the mock only understands the backend's own condition expression",
        );
        let expected = request["ExpressionAttributeValues"][":expected"]["N"]
            .as_str()
            .and_then(|n| n.parse::<u64>().ok())
            .expect("condition supplies :expected");
        let satisfied = match &existing {
            None => true,
            Some(current) => number_attr(current, "version") == Some(expected),
        };
        if !satisfied {
            return service_error(
                StatusCode::BAD_REQUEST,
                "com.amazonaws.dynamodb.v20120810#ConditionalCheckFailedException",
                "the conditional request failed",
            );
        }
    }

    table.items.insert((pk, sk), item);
    axum::Json(json!({})).into_response()
}

fn query(mock: &DynamoMock, request: &Value) -> Response {
    let pk = request["ExpressionAttributeValues"][":pk"]["S"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        request["KeyConditionExpression"].as_str(),
        Some("pk = :pk"),
        "the mock only understands the backend's own key condition",
    );
    let table = mock.inner.lock().unwrap();
    let items: Vec<&Item> = table
        .items
        .iter()
        .filter(|((p, _), _)| *p == pk)
        .map(|(_, item)| item)
        .collect();
    axum::Json(json!({ "Items": items, "Count": items.len() })).into_response()
}

fn delete_item(mock: &DynamoMock, request: &Value) -> Response {
    let (pk, sk) = key_of(request, "Key");
    mock.inner.lock().unwrap().items.remove(&(pk, sk));
    axum::Json(json!({})).into_response()
}

fn service_error(status: StatusCode, error_type: &str, message: &str) -> Response {
    (
        status,
        [("x-amzn-errortype", error_type.to_owned())],
        axum::Json(json!({ "__type": error_type, "message": message })),
    )
        .into_response()
}
