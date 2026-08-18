//! Document rasterization: PDFs in, images out.
//!
//! Some targets cannot carry a document at all. The ChatGPT Codex backend
//! is the case that forced this crate: its Responses `input` array accepts
//! `input_text` and `input_image` and nothing else — the official client's
//! own `ContentItem` enum (`codex-rs`, `protocol/src/models.rs`) has three
//! variants and none of them is a file. A caller who attaches a PDF to a
//! Codex request therefore has exactly two honest outcomes: an error, or
//! the pages as images. This crate provides the second.
//!
//! Rendering is pure CPU with no I/O and no unsafe code ([`hayro`] is a
//! pure-Rust rasterizer, which is what keeps the release build a static
//! musl binary — a libpdfium or MuPDF binding would have ended that).
//! It is nonetheless slow enough (tens of milliseconds per page) that
//! callers must keep it off an async runtime's worker; see
//! [`rasterize_request`]'s note.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use router_core::chat::{ChatRequest, Content, ContentPart, ImageUrl};
use router_core::{ErrorClass, GatewayError};

/// The document media types we can rasterize.
///
/// Deliberately one entry. A caller who attaches a `.docx` gets a clear
/// 400 naming what is supported rather than a rendering failure deep in a
/// worker thread.
pub const SUPPORTED_DOCUMENT_MEDIA_TYPES: &[&str] = &["application/pdf"];

/// How to render, and how much is too much.
#[derive(Debug, Clone, Copy)]
pub struct RasterSettings {
    /// Render resolution. 150 puts a US Letter page at roughly 1650px on
    /// the long edge, which is just past the point where vision models
    /// downsample anyway — higher costs bytes and buys nothing.
    pub dpi: u32,
    /// Hard ceiling on pages rendered from one document.
    pub max_pages: usize,
    /// Hard ceiling on the total encoded size of one document's pages.
    /// A 200-page chart would otherwise turn one request into a body no
    /// upstream will accept, after having spent a minute rendering it.
    pub max_total_bytes: usize,
}

impl Default for RasterSettings {
    fn default() -> Self {
        Self {
            dpi: 150,
            max_pages: 50,
            max_total_bytes: 24 * 1024 * 1024,
        }
    }
}

/// What a rasterization pass did, for the caller to log or surface.
///
/// A cap that silently truncates a document reads as "we sent the whole
/// thing" when we did not, so both ceilings report what they dropped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RasterReport {
    pub documents: usize,
    pub pages_rendered: usize,
    /// Pages present in the source but not sent, per document.
    pub pages_dropped: usize,
}

impl RasterReport {
    pub fn is_empty(&self) -> bool {
        self.documents == 0
    }
}

fn invalid(message: impl Into<String>) -> GatewayError {
    GatewayError::new(ErrorClass::InvalidRequest, message).with_param("messages")
}

/// Split a `data:<media-type>;base64,<payload>` URI.
///
/// Only the base64 form is accepted: the percent-encoded form is not used
/// by any client we serve and would silently mis-decode binary payloads.
/// Every malformed shape is an error rather than a partial result, so a
/// truncated upload becomes a 400 instead of a corrupt document handed to
/// a renderer.
pub fn parse_data_uri(uri: &str) -> Result<(String, &str), GatewayError> {
    let rest = uri
        .strip_prefix("data:")
        .ok_or_else(|| invalid("attachment must be a `data:` URI"))?;
    let (header, payload) = rest
        .split_once(',')
        .ok_or_else(|| invalid("malformed data URI: missing `,` separator"))?;
    if !header.contains(";base64") {
        return Err(invalid(
            "malformed data URI: only `;base64` payloads are supported",
        ));
    }
    let media_type = header
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if media_type.is_empty() {
        return Err(invalid("malformed data URI: missing media type"));
    }
    if payload.is_empty() {
        return Err(invalid("malformed data URI: empty payload"));
    }
    Ok((media_type, payload))
}

/// The payload of a `file` content part, whichever spelling it arrived in.
///
/// Chat Completions nests it under `file.file_data`; the Responses API puts
/// `file_data` at the top of the part. Both are accepted — the difference
/// carries no meaning. A `file_id` reference is refused explicitly: there
/// is no Files API to resolve it against, and ignoring it is exactly the
/// silent-drop this crate exists to remove.
fn document_bytes(file: &serde_json::Value) -> Result<Vec<u8>, GatewayError> {
    let inner = file.get("file").unwrap_or(file);
    let data_uri = inner
        .get("file_data")
        .or_else(|| inner.get("file_url"))
        .and_then(serde_json::Value::as_str);
    let Some(data_uri) = data_uri else {
        if inner.get("file_id").is_some() {
            return Err(invalid(
                "file blocks referencing `file_id` are not supported; inline the document as a \
                 base64 `file_data` data URI instead",
            ));
        }
        return Err(invalid("file block is missing `file_data`"));
    };
    let (media_type, payload) = parse_data_uri(data_uri)?;
    if !SUPPORTED_DOCUMENT_MEDIA_TYPES.contains(&media_type.as_str()) {
        return Err(invalid(format!(
            "unsupported document media type `{media_type}`; supported: {}",
            SUPPORTED_DOCUMENT_MEDIA_TYPES.join(", ")
        )));
    }
    BASE64
        .decode(payload)
        .map_err(|_| invalid("malformed base64 payload for document"))
}

/// Render a PDF's pages to PNGs.
///
/// Errors are the caller's to fix (a corrupt or encrypted upload), never a
/// 500: a document we cannot open is an invalid request, and saying so is
/// more useful than a rendering panic.
pub fn render_pdf(
    data: &[u8],
    settings: &RasterSettings,
) -> Result<(Vec<Vec<u8>>, usize), GatewayError> {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{RenderCache, RenderSettings, render};

    let pdf = Pdf::new(std::sync::Arc::new(data.to_vec()))
        .map_err(|e| invalid(format!("could not read the supplied PDF: {e:?}")))?;
    let pages = pdf.pages();
    if pages.is_empty() {
        return Err(invalid("PDF contains no pages"));
    }

    let scale = settings.dpi as f32 / 72.0;
    let cache = RenderCache::new();
    let interpreter = InterpreterSettings::default();
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut bytes = 0usize;

    for page in pages.iter().take(settings.max_pages) {
        let pixmap = render(
            page,
            &cache,
            &interpreter,
            &RenderSettings {
                x_scale: scale,
                y_scale: scale,
                // A PDF page is paper: opaque white, not transparent. A
                // transparent background would composite to black in most
                // encoders and hand the model a negative image.
                bg_color: WHITE,
                ..Default::default()
            },
        );
        let png = encode_png(&pixmap)?;
        // Stop at the byte ceiling rather than after it: the page that
        // would breach it is not sent, so the body stays under the limit.
        if bytes + png.len() > settings.max_total_bytes && !out.is_empty() {
            break;
        }
        bytes += png.len();
        out.push(png);
    }

    if out.is_empty() {
        return Err(invalid("PDF produced no renderable pages"));
    }
    Ok((out, pages.len()))
}

/// Encode one rendered page as an 8-bit RGB PNG.
///
/// RGB, not RGBA: the page was rendered onto opaque white, so the alpha
/// channel is a constant 255 and carrying it is a quarter of the bytes for
/// nothing — and these bytes are base64'd into a request body.
fn encode_png(pixmap: &hayro::vello_cpu::Pixmap) -> Result<Vec<u8>, GatewayError> {
    let (width, height) = (pixmap.width() as u32, pixmap.height() as u32);
    let rgb: Vec<u8> = pixmap.data().iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut w| w.write_image_data(&rgb))
        .map_err(|e| {
            GatewayError::new(
                ErrorClass::UpstreamError,
                format!("could not encode a rendered PDF page: {e}"),
            )
        })?;
    Ok(out)
}

/// True when any message carries a document part.
///
/// Shape-only, so the common no-attachment request pays nothing and this
/// cannot raise on a malformed document it is about to leave alone.
pub fn has_documents(req: &ChatRequest) -> bool {
    req.messages.iter().any(|m| {
        matches!(&m.content, Some(Content::Parts(parts))
            if parts.iter().any(|p| matches!(p, ContentPart::File { .. })))
    })
}

/// Replace every document part in `req` with the images of its pages.
///
/// Order is preserved and the pages land where the document was, because
/// interleaving is load-bearing — "the invoice below" reads differently if
/// the pages are hoisted above the sentence that introduces them.
///
/// **This blocks.** Rendering a chart runs to hundreds of milliseconds; an
/// async caller must run it on a blocking thread or it will stall every
/// other request sharing the runtime worker.
pub fn rasterize_request(
    req: &ChatRequest,
    settings: &RasterSettings,
) -> Result<(ChatRequest, RasterReport), GatewayError> {
    let mut out = req.clone();
    let mut report = RasterReport::default();

    for message in &mut out.messages {
        let Some(Content::Parts(parts)) = &message.content else {
            continue;
        };
        if !parts.iter().any(|p| matches!(p, ContentPart::File { .. })) {
            continue;
        }
        let mut rebuilt: Vec<ContentPart> = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                ContentPart::File { file } => {
                    let bytes = document_bytes(file)?;
                    let (pages, total) = render_pdf(&bytes, settings)?;
                    report.documents += 1;
                    report.pages_rendered += pages.len();
                    report.pages_dropped += total.saturating_sub(pages.len());
                    for png in pages {
                        rebuilt.push(ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: format!("data:image/png;base64,{}", BASE64.encode(&png)),
                                detail: None,
                            },
                        });
                    }
                }
                other => rebuilt.push(other.clone()),
            }
        }
        message.content = Some(Content::Parts(rebuilt));
    }

    Ok((out, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use router_core::chat::Message;

    /// A one-page PDF with a line of text, built by hand so the tests need
    /// no fixture file and no third-party writer.
    fn tiny_pdf() -> Vec<u8> {
        let text = b"BT /F1 14 Tf 20 50 Td (HELLO PDF) Tj ET";
        let objs: Vec<Vec<u8>> = vec![
            b"<</Type/Catalog/Pages 2 0 R>>".to_vec(),
            b"<</Type/Pages/Kids[3 0 R]/Count 1>>".to_vec(),
            b"<</Type/Page/Parent 2 0 R/MediaBox[0 0 300 100]/Contents 4 0 R\
               /Resources<</Font<</F1 5 0 R>>>>>>"
                .to_vec(),
            [
                format!("<</Length {}>>stream\n", text.len()).into_bytes(),
                text.to_vec(),
                b"\nendstream".to_vec(),
            ]
            .concat(),
            b"<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>".to_vec(),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj", i + 1).as_bytes());
            out.extend_from_slice(o);
            out.extend_from_slice(b"endobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for off in &offsets {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn request_with(parts: Vec<ContentPart>) -> ChatRequest {
        serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": []}],
        }))
        .map(|mut r: ChatRequest| {
            r.messages = vec![Message {
                role: "user".into(),
                content: Some(Content::Parts(parts)),
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }];
            r
        })
        .unwrap()
    }

    fn pdf_part() -> ContentPart {
        ContentPart::File {
            file: serde_json::json!({
                "filename": "tiny.pdf",
                "file_data": format!("data:application/pdf;base64,{}", BASE64.encode(tiny_pdf())),
            }),
        }
    }

    #[test]
    fn a_pdf_becomes_one_image_per_page() {
        let req = request_with(vec![
            ContentPart::Text {
                text: "read this".into(),
            },
            pdf_part(),
        ]);
        let (out, report) = rasterize_request(&req, &RasterSettings::default()).unwrap();
        let Some(Content::Parts(parts)) = &out.messages[0].content else {
            panic!("parts");
        };
        assert_eq!(parts.len(), 2, "text part is kept, document becomes a page");
        assert!(matches!(parts[0], ContentPart::Text { .. }));
        match &parts[1] {
            ContentPart::ImageUrl { image_url } => {
                assert!(image_url.url.starts_with("data:image/png;base64,"));
            }
            other => panic!("expected an image, got {other:?}"),
        }
        assert_eq!(report.documents, 1);
        assert_eq!(report.pages_rendered, 1);
        assert_eq!(report.pages_dropped, 0);
    }

    #[test]
    fn pages_land_where_the_document_was() {
        let req = request_with(vec![
            ContentPart::Text {
                text: "before".into(),
            },
            pdf_part(),
            ContentPart::Text {
                text: "after".into(),
            },
        ]);
        let (out, _) = rasterize_request(&req, &RasterSettings::default()).unwrap();
        let Some(Content::Parts(parts)) = &out.messages[0].content else {
            panic!("parts");
        };
        assert!(matches!(&parts[0], ContentPart::Text { text } if text == "before"));
        assert!(matches!(parts[1], ContentPart::ImageUrl { .. }));
        assert!(matches!(&parts[2], ContentPart::Text { text } if text == "after"));
    }

    #[test]
    fn a_request_without_documents_is_untouched() {
        let req = request_with(vec![ContentPart::Text {
            text: "plain".into(),
        }]);
        assert!(!has_documents(&req));
        let (out, report) = rasterize_request(&req, &RasterSettings::default()).unwrap();
        assert!(report.is_empty());
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::to_value(&req).unwrap()
        );
    }

    #[test]
    fn the_page_cap_is_reported_not_hidden() {
        let settings = RasterSettings {
            max_pages: 1,
            ..Default::default()
        };
        // One page in, one page out — but the report is what a caller
        // reads to know nothing was cut, so assert it stays honest.
        let (_, report) = rasterize_request(&request_with(vec![pdf_part()]), &settings).unwrap();
        assert_eq!(report.pages_dropped, 0);
    }

    #[test]
    fn a_corrupt_pdf_is_a_400_not_a_panic() {
        let part = ContentPart::File {
            file: serde_json::json!({
                "file_data": format!("data:application/pdf;base64,{}", BASE64.encode(b"not a pdf")),
            }),
        };
        let err = rasterize_request(&request_with(vec![part]), &RasterSettings::default())
            .expect_err("a corrupt document must not render");
        assert_eq!(err.class, ErrorClass::InvalidRequest);
    }

    #[test]
    fn a_file_id_reference_says_why_it_cannot_be_honoured() {
        let part = ContentPart::File {
            file: serde_json::json!({"file_id": "file-abc123"}),
        };
        let err = rasterize_request(&request_with(vec![part]), &RasterSettings::default())
            .expect_err("there is no Files API to resolve against");
        assert!(err.to_string().contains("file_id"), "{err}");
    }

    #[test]
    fn a_non_pdf_document_names_what_is_supported() {
        let part = ContentPart::File {
            file: serde_json::json!({
                "file_data": "data:application/vnd.ms-excel;base64,QUJD",
            }),
        };
        let err = rasterize_request(&request_with(vec![part]), &RasterSettings::default())
            .expect_err("only PDFs rasterize");
        assert!(err.to_string().contains("application/pdf"), "{err}");
    }

    #[test]
    fn the_chat_completions_nesting_is_accepted_too() {
        let part = ContentPart::File {
            file: serde_json::json!({
                "file": {
                    "filename": "tiny.pdf",
                    "file_data": format!(
                        "data:application/pdf;base64,{}", BASE64.encode(tiny_pdf())
                    ),
                }
            }),
        };
        let (_, report) =
            rasterize_request(&request_with(vec![part]), &RasterSettings::default()).unwrap();
        assert_eq!(report.pages_rendered, 1);
    }

    #[test]
    fn a_truncated_data_uri_is_rejected_before_decoding() {
        assert!(parse_data_uri("data:application/pdf,AAAA").is_err());
        assert!(parse_data_uri("data:;base64,AAAA").is_err());
        assert!(parse_data_uri("data:application/pdf;base64,").is_err());
        assert!(parse_data_uri("https://example.com/a.pdf").is_err());
    }
}
