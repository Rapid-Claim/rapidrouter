//! Subscription quota: how long a seat stays benched, and what the
//! provider told us about its remaining window.
//!
//! Metered API keys fail and recover on our schedule — the breaker's
//! configured cooldown. A subscription seat does not: it is out until the
//! provider's rolling window rolls, and that window is reported in the
//! response. Guessing costs real money in wasted attempts, so everything
//! here is about honoring what the provider actually said.
//!
//! Every field name below was captured from a live response (2026-08-15);
//! both surfaces are undocumented, so every read is defensive and a
//! payload we no longer recognize degrades to "no information" rather than
//! to a wrong number.

use std::time::Duration;

/// Ceiling on an honoured bench. A weekly Codex window reports ~6.5 days
/// and an exhausted Anthropic 5h window ~55 minutes; benching a seat for
/// days on the strength of one response body is more trust than a single
/// upstream field has earned, and runtime health does not survive a
/// restart anyway. A day keeps an exhausted seat out for essentially the
/// whole window at a cost of at most one wasted probe per seat per day.
pub const MAX_BENCH: Duration = Duration::from_secs(24 * 60 * 60);

/// Floor. A zero or negative window would re-admit the seat instantly,
/// which just earns another 429.
pub const MIN_BENCH: Duration = Duration::from_secs(1);

/// Upper bound of the random padding added to every bench, as a fraction.
///
/// Cooldowns are set by upstream events that hit every seat at once — a
/// shared quota window resetting, a backend blip — so an unjittered bench
/// re-admits the whole pool in the same instant: they all retry, all 429,
/// and all bench again together. The padding is **one-sided**, because the
/// input is the provider's own reset time and coming back early is never
/// rewarded.
pub const JITTER_RATIO: f64 = 0.10;

/// Clamp a reported reset window into an honourable bench.
pub fn clamp(window: Duration) -> Duration {
    window.clamp(MIN_BENCH, MAX_BENCH)
}

/// Clamp, then pad by 0–`JITTER_RATIO` using a caller-supplied uniform
/// sample in `[0, 1)`. Pure, so the jitter schedule is testable; callers
/// pass `fastrand::f64()`.
pub fn bench_for(window: Duration, jitter_sample: f64) -> Duration {
    let base = clamp(window);
    let sample = if jitter_sample.is_finite() {
        jitter_sample.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let padded = base.mul_f64(1.0 + sample * JITTER_RATIO);
    // Padding can only ever make a seat safer to retry.
    padded.max(base).min(MAX_BENCH.mul_f64(1.0 + JITTER_RATIO))
}

/// Seconds to wait, parsed from a `retry-after` header value.
///
/// Only the delta-seconds form is accepted. The HTTP-date form is legal
/// but neither backend sends it, and half-parsing dates here would be more
/// code than the case deserves — an unparsable value means "no
/// information", and the body is consulted next.
pub fn retry_after_header(value: &str) -> Option<Duration> {
    let secs: f64 = value.trim().parse().ok()?;
    if !secs.is_finite() {
        return None;
    }
    Some(Duration::from_secs_f64(secs.max(0.0)))
}

/// Keys an error body may carry the reset window under, most specific
/// first. The union across both backends: a key an upstream never sends
/// costs nothing to look for.
const RELATIVE_KEYS: [&str; 3] = ["resets_in_seconds", "retry_after_seconds", "retry_after"];
/// Absolute-epoch spellings of the same thing.
const ABSOLUTE_KEYS: [&str; 2] = ["resets_at", "reset_at"];

/// Seconds to wait, dug out of a parsed JSON error body.
///
/// Searched at the top level **and** inside a nested `error` object. This
/// nesting is not a nicety: the ChatGPT Codex backend answers an exhausted
/// seat with
///
/// ```json
/// {"error": {"type": "usage_limit_reached", "resets_in_seconds": 380612}}
/// ```
///
/// and sends no `retry-after` header at all (verified live, 2026-08-15).
/// Reading only the top level yields `None` for every rate-limited Codex
/// seat, which silently degrades the whole pool to a fixed short cooldown:
/// a seat out of *weekly* quota gets re-probed every few minutes, 429s,
/// and re-benches, burning one caller retry each cycle.
///
/// `now_epoch_secs` interprets the absolute spellings (`resets_at`), which
/// the same body also carries.
pub fn retry_after_body(body: &serde_json::Value, now_epoch_secs: u64) -> Option<Duration> {
    for scope in [body, body.get("error").unwrap_or(&serde_json::Value::Null)] {
        for key in RELATIVE_KEYS {
            if let Some(secs) = scope.get(key).and_then(finite_number) {
                return Some(Duration::from_secs_f64(secs.max(0.0)));
            }
        }
        for key in ABSOLUTE_KEYS {
            if let Some(at) = scope.get(key).and_then(finite_number) {
                let remaining = at - now_epoch_secs as f64;
                return Some(Duration::from_secs_f64(remaining.max(0.0)));
            }
        }
    }
    None
}

/// A JSON number (or numeric string) that is actually a number.
///
/// `serde_json` will happily hand back a `Value::Bool`, and "retry in 1
/// second" is never what a boolean in this field meant. Non-finite values
/// are refused too: they survive the parse only to blow up the duration
/// conversion, turning a rate-limit response into a hard 500 with no
/// retry — the opposite of this function's contract.
fn finite_number(value: &serde_json::Value) -> Option<f64> {
    let n = match value {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    n.is_finite().then_some(n)
}

/// One rolling quota window as the provider reports it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Fraction of the window consumed. `1.0` means exhausted; both
    /// backends can report slightly over.
    pub utilization: f64,
    /// Seconds until the window rolls, when the provider says.
    pub resets_in: Option<Duration>,
    /// Nominal window length, when the provider says (Codex reports it;
    /// Anthropic's are fixed at 5h/7d).
    pub length: Option<Duration>,
    /// True when this window is the one currently refusing requests.
    pub rejected: bool,
}

/// The quota picture from one response: a short window and a long one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Quota {
    /// Anthropic's 5h; Codex's `primary`.
    pub primary: Option<Window>,
    /// Anthropic's 7d; Codex's `secondary`.
    pub secondary: Option<Window>,
}

impl Quota {
    pub fn is_empty(&self) -> bool {
        self.primary.is_none() && self.secondary.is_none()
    }

    /// The most-consumed window, for a single "how close is this seat to
    /// the edge" gauge.
    pub fn peak_utilization(&self) -> Option<f64> {
        [self.primary, self.secondary]
            .into_iter()
            .flatten()
            .map(|w| w.utilization)
            .fold(None, |acc: Option<f64>, u| {
                Some(acc.map_or(u, |a| a.max(u)))
            })
    }
}

/// Read Anthropic's unified rate-limit headers.
///
/// Shape captured live from a subscription token (2026-08-15):
///
/// ```text
/// anthropic-ratelimit-unified-5h-utilization: 1.01
/// anthropic-ratelimit-unified-5h-status: rejected
/// anthropic-ratelimit-unified-5h-reset: 1786819200      (absolute epoch)
/// anthropic-ratelimit-unified-7d-utilization: 0.22
/// anthropic-ratelimit-unified-status: rejected
/// anthropic-ratelimit-unified-representative-claim: five_hour
/// ```
///
/// `reset` is an **absolute epoch**, unlike Codex's relative seconds,
/// hence `now_epoch_secs`.
pub fn anthropic_quota(header: impl Fn(&str) -> Option<String>, now_epoch_secs: u64) -> Quota {
    let window = |prefix: &str, length: Duration| -> Option<Window> {
        let utilization: f64 =
            header(&format!("anthropic-ratelimit-unified-{prefix}-utilization"))?
                .trim()
                .parse()
                .ok()?;
        let resets_in = header(&format!("anthropic-ratelimit-unified-{prefix}-reset"))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|at| Duration::from_secs(at.saturating_sub(now_epoch_secs)));
        let rejected = header(&format!("anthropic-ratelimit-unified-{prefix}-status"))
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("rejected"));
        Some(Window {
            utilization,
            resets_in,
            length: Some(length),
            rejected,
        })
    };
    Quota {
        primary: window("5h", Duration::from_secs(5 * 3600)),
        secondary: window("7d", Duration::from_secs(7 * 24 * 3600)),
    }
}

/// Read the ChatGPT Codex backend's `x-codex-*` rate-limit headers.
///
/// Shape captured live (2026-08-15):
///
/// ```text
/// x-codex-primary-used-percent: 100
/// x-codex-primary-reset-after-seconds: 380613           (relative)
/// x-codex-primary-window-minutes: 10080
/// x-codex-secondary-used-percent: 0
/// x-codex-plan-type: pro
/// ```
///
/// `used-percent` is 0–100, not a 0–1 fraction; it is normalized here so
/// both providers report one shape. An empty value (the backend sends
/// `x-codex-secondary-reset-at:` with nothing after it when there is no
/// secondary window) parses as absent, not as zero.
pub fn codex_quota(header: impl Fn(&str) -> Option<String>) -> Quota {
    let window = |prefix: &str| -> Option<Window> {
        let used: f64 = non_empty(header(&format!("x-codex-{prefix}-used-percent"))?)?
            .parse()
            .ok()?;
        let resets_in = header(&format!("x-codex-{prefix}-reset-after-seconds"))
            .and_then(non_empty)
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        let length = header(&format!("x-codex-{prefix}-window-minutes"))
            .and_then(non_empty)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|m| *m > 0)
            .map(|m| Duration::from_secs(m * 60));
        Some(Window {
            utilization: used / 100.0,
            resets_in,
            length,
            // The Codex headers carry no per-window status; exhaustion is
            // reported by the 429 body, and 100% is the observable proxy.
            rejected: used >= 100.0,
        })
    };
    Quota {
        primary: window("primary"),
        secondary: window("secondary"),
    }
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamp_bounds_the_window() {
        assert_eq!(clamp(Duration::ZERO), MIN_BENCH);
        assert_eq!(clamp(Duration::from_secs(600)), Duration::from_secs(600));
        // A weekly Codex window, as actually reported.
        assert_eq!(clamp(Duration::from_secs(380_612)), MAX_BENCH);
    }

    #[test]
    fn jitter_only_ever_pads() {
        let base = Duration::from_secs(300);
        for sample in [0.0, 0.25, 0.5, 0.99] {
            let benched = bench_for(base, sample);
            assert!(benched >= base, "jitter must never return early");
            assert!(benched <= base.mul_f64(1.0 + JITTER_RATIO));
        }
    }

    #[test]
    fn jitter_survives_a_garbage_sample() {
        let base = Duration::from_secs(60);
        assert_eq!(bench_for(base, f64::NAN), base);
        assert!(bench_for(base, 12.0) <= base.mul_f64(1.0 + JITTER_RATIO));
    }

    #[test]
    fn retry_after_header_reads_delta_seconds() {
        assert_eq!(
            retry_after_header("3311"),
            Some(Duration::from_secs(3311)),
            "the value Anthropic actually sent on an exhausted 5h window"
        );
        assert_eq!(
            retry_after_header(" 12.5 "),
            Some(Duration::from_secs_f64(12.5))
        );
        assert_eq!(retry_after_header("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(retry_after_header(""), None);
    }

    #[test]
    fn retry_after_body_reads_the_nested_codex_shape() {
        // Verbatim from a live 429 (2026-08-15). No retry-after header
        // accompanied it — this nesting is the only source.
        let body = json!({
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "pro",
                "resets_at": 1_787_196_530u64,
                "resets_in_seconds": 380_612u64
            }
        });
        assert_eq!(
            retry_after_body(&body, 1_786_815_918),
            Some(Duration::from_secs(380_612))
        );
    }

    #[test]
    fn retry_after_body_falls_back_to_absolute_epoch() {
        let body = json!({"error": {"resets_at": 1_787_196_530u64}});
        assert_eq!(
            retry_after_body(&body, 1_786_815_918),
            Some(Duration::from_secs(380_612))
        );
        // A reset already in the past is zero, not a huge negative.
        let past = json!({"resets_at": 10u64});
        assert_eq!(retry_after_body(&past, 100), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_body_refuses_junk() {
        assert_eq!(retry_after_body(&json!({"retry_after": true}), 0), None);
        assert_eq!(retry_after_body(&json!({"retry_after": "soon"}), 0), None);
        assert_eq!(retry_after_body(&json!([1, 2, 3]), 0), None);
        assert_eq!(retry_after_body(&json!({}), 0), None);
        // A numeric string is a proxy rewriting a number; still usable.
        assert_eq!(
            retry_after_body(&json!({"retry_after": " 30 "}), 0),
            Some(Duration::from_secs(30))
        );
    }

    fn header_map(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn anthropic_headers_parse_as_captured() {
        // Verbatim from a live 429 against a Claude Max subscription token.
        let h = header_map(&[
            ("anthropic-ratelimit-unified-5h-utilization", "1.01"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", "1786819200"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.22"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-reset", "1787389200"),
        ]);
        let q = anthropic_quota(h, 1_786_815_889);
        let five_hour = q.primary.expect("5h window");
        assert!((five_hour.utilization - 1.01).abs() < 1e-9);
        assert!(five_hour.rejected);
        assert_eq!(five_hour.resets_in, Some(Duration::from_secs(3311)));
        let weekly = q.secondary.expect("7d window");
        assert!(!weekly.rejected);
        assert_eq!(q.peak_utilization(), Some(1.01));
    }

    #[test]
    fn codex_headers_parse_as_captured() {
        // Verbatim from a live response; note the empty secondary values.
        let h = header_map(&[
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-after-seconds", "380613"),
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-secondary-used-percent", "0"),
            ("x-codex-secondary-reset-at", ""),
            ("x-codex-secondary-window-minutes", "0"),
        ]);
        let q = codex_quota(h);
        let primary = q.primary.expect("primary window");
        assert_eq!(primary.utilization, 1.0, "100 percent normalizes to 1.0");
        assert!(primary.rejected);
        assert_eq!(primary.length, Some(Duration::from_secs(7 * 24 * 3600)));
        let secondary = q.secondary.expect("secondary window");
        assert_eq!(secondary.utilization, 0.0);
        assert_eq!(secondary.length, None, "a zero-minute window is absent");
    }

    #[test]
    fn missing_headers_yield_no_quota() {
        let none = codex_quota(|_| None);
        assert!(none.is_empty());
        assert!(anthropic_quota(|_| None, 0).is_empty());
        assert_eq!(none.peak_utilization(), None);
    }
}
