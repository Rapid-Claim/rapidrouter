//! The fast path is only allowed to exist because it provably agrees with
//! the full parser: for any JSON document, a successful probe must match
//! what serde_json sees, and a splice must produce the document serde
//! would have produced by mutating `model`.

use bytes::Bytes;
use proptest::prelude::*;
use router_core::json::{probe, splice_model};
use serde_json::{Value, json};

/// Arbitrary JSON values, deep enough to exercise nested skipping.
fn arb_json(depth: u32) -> BoxedStrategy<Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(Value::from),
        ".{0,30}".prop_map(Value::from),
    ];
    leaf.prop_recursive(depth, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::from),
            prop::collection::btree_map(".{0,12}", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
    .boxed()
}

/// Top-level request-shaped objects: a `model` string, sometimes a
/// `stream` bool, and arbitrary other fields.
fn arb_request() -> impl Strategy<Value = Value> {
    (
        ".{0,40}",
        prop::option::of(any::<bool>()),
        prop::collection::btree_map("[a-z_]{1,10}", arb_json(3), 0..5),
    )
        .prop_map(|(model, stream, extra)| {
            let mut obj = serde_json::Map::new();
            for (k, v) in extra {
                if k != "model" && k != "stream" {
                    obj.insert(k, v);
                }
            }
            obj.insert("model".into(), json!(model));
            if let Some(s) = stream {
                obj.insert("stream".into(), json!(s));
            }
            Value::Object(obj)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A successful probe agrees with serde_json on both fields.
    #[test]
    fn probe_matches_serde(request in arb_request()) {
        let body = serde_json::to_vec(&request).unwrap();
        if let Some(got) = probe(&body) {
            prop_assert_eq!(Some(got.model.as_str()), request["model"].as_str());
            prop_assert_eq!(got.stream, request.get("stream").and_then(Value::as_bool));
        }
        // `None` is always legal: it means the slow path runs instead.
    }

    /// Probing serde-serialized request objects must not fall back: the
    /// fast path has to actually cover the traffic it exists for.
    #[test]
    fn probe_succeeds_on_wellformed_requests(request in arb_request()) {
        let body = serde_json::to_vec(&request).unwrap();
        prop_assert!(probe(&body).is_some(), "fell back on: {}", String::from_utf8_lossy(&body));
    }

    /// Splicing equals the parse -> mutate -> serialize slow path,
    /// compared as documents.
    #[test]
    fn splice_matches_full_rewrite(request in arb_request(), new_model in ".{0,40}") {
        let body = Bytes::from(serde_json::to_vec(&request).unwrap());
        let probed = probe(&body).unwrap();
        let spliced = splice_model(&body, probed.model_span, &new_model);

        let mut expected = request.clone();
        expected["model"] = json!(new_model);
        let got: Value = serde_json::from_slice(&spliced).unwrap();
        prop_assert_eq!(got, expected);
    }

    /// The probe never panics on arbitrary bytes.
    #[test]
    fn probe_total_on_garbage(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let _ = probe(&bytes);
    }
}
