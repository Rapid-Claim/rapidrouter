//! AWS Signature Version 4 request signing, hand-rolled: HMAC-SHA256
//! chain over a canonical request. Only what Bedrock needs — POST with a
//! JSON body, `host`/`x-amz-date` signed headers.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub struct SigningParams<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
    pub host: &'a str,
    pub method: &'a str,
    /// Already percent-encoded path.
    pub canonical_path: &'a str,
    pub query: &'a str,
    pub payload: &'a [u8],
}

pub struct Signature {
    pub authorization: String,
    pub amz_content_sha256: String,
}

pub fn sign(params: &SigningParams<'_>) -> Signature {
    let date = &params.amz_date[..8];
    let payload_hash = hex(&Sha256::digest(params.payload));

    let canonical_headers = format!(
        "content-type:application/json\nhost:{}\nx-amz-date:{}\n",
        params.host, params.amz_date
    );
    let signed_headers = "content-type;host;x-amz-date";
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        params.method,
        params.canonical_path,
        params.query,
        canonical_headers,
        signed_headers,
        payload_hash,
    );

    let scope = format!("{date}/{}/{}/aws4_request", params.region, params.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        params.amz_date,
        hex(&Sha256::digest(canonical_request.as_bytes())),
    );

    let k_date = hmac(
        format!("AWS4{}", params.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, params.region.as_bytes());
    let k_service = hmac(&k_region, params.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, string_to_sign.as_bytes()));

    Signature {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            params.access_key_id,
        ),
        amz_content_sha256: payload_hash,
    }
}

/// Percent-encode a path segment per RFC 3986 unreserved rules (AWS
/// canonical form) — Bedrock model ids carry `:` and `.`.
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub fn amz_date_now() -> String {
    // UTC timestamp without pulling in a date library: derive from the
    // Unix epoch by civil-calendar arithmetic.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after 1970")
        .as_secs();
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AWS's published SigV4 test vector (get-vanilla, adapted to our
    /// fixed header set is not possible; instead verify the HMAC chain
    /// against the documented example values).
    #[test]
    fn signing_key_chain_matches_aws_example() {
        // From AWS docs: secret wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY,
        // date 20120215, region us-east-1, service iam.
        let k_date = hmac(b"AWS4wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", b"20120215");
        let k_region = hmac(&k_date, b"us-east-1");
        let k_service = hmac(&k_region, b"iam");
        let k_signing = hmac(&k_service, b"aws4_request");
        assert_eq!(
            hex(&k_signing),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn path_encoding_handles_model_ids() {
        assert_eq!(
            encode_path_segment("anthropic.claude-3-haiku-20240307-v1:0"),
            "anthropic.claude-3-haiku-20240307-v1%3A0"
        );
    }

    #[test]
    fn amz_date_shape() {
        let d = amz_date_now();
        assert_eq!(d.len(), 16);
        assert!(d.ends_with('Z'));
        assert!(d.starts_with("20"));
        assert_eq!(&d[8..9], "T");
    }
}
