const SNIFF_LEN: usize = 512;
const OCTET_STREAM: &str = "application/octet-stream";
const HTML: &str = "text/html; charset=utf-8";
const PLAIN: &str = "text/plain; charset=utf-8";
const XML: &str = "text/xml; charset=utf-8";
const FONT_OBJECT_MASK: &[u8] = b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\xFF\xFF";
const FONT_OBJECT_PATTERN: &[u8] = b"\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00LP";

enum Signature {
    Html(&'static [u8]),
    Exact(&'static [u8], &'static str),
    Masked {
        mask: &'static [u8],
        pattern: &'static [u8],
        skip_whitespace: bool,
        content_type: &'static str,
    },
    Mp4,
    Text,
}

use Signature::{Exact, Html, Masked, Mp4, Text};

const fn masked(mask: &'static [u8], pattern: &'static [u8], content_type: &'static str) -> Signature {
    Masked {
        mask,
        pattern,
        skip_whitespace: false,
        content_type,
    }
}

static SIGNATURES: [Signature; 51] = [
    Html(b"<!DOCTYPE HTML"),
    Html(b"<HTML"),
    Html(b"<HEAD"),
    Html(b"<SCRIPT"),
    Html(b"<IFRAME"),
    Html(b"<H1"),
    Html(b"<DIV"),
    Html(b"<FONT"),
    Html(b"<TABLE"),
    Html(b"<A"),
    Html(b"<STYLE"),
    Html(b"<TITLE"),
    Html(b"<B"),
    Html(b"<BODY"),
    Html(b"<BR"),
    Html(b"<P"),
    Html(b"<!--"),
    Masked {
        mask: b"\xFF\xFF\xFF\xFF\xFF",
        pattern: b"<?xml",
        skip_whitespace: true,
        content_type: XML,
    },
    Exact(b"%PDF-", "application/pdf"),
    Exact(b"%!PS-Adobe-", "application/postscript"),
    masked(b"\xFF\xFF\x00\x00", b"\xFE\xFF\x00\x00", "text/plain; charset=utf-16be"),
    masked(b"\xFF\xFF\x00\x00", b"\xFF\xFE\x00\x00", "text/plain; charset=utf-16le"),
    masked(b"\xFF\xFF\xFF\x00", b"\xEF\xBB\xBF\x00", PLAIN),
    Exact(b"\x00\x00\x01\x00", "image/x-icon"),
    Exact(b"\x00\x00\x02\x00", "image/x-icon"),
    Exact(b"BM", "image/bmp"),
    Exact(b"GIF87a", "image/gif"),
    Exact(b"GIF89a", "image/gif"),
    masked(
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00WEBPVP",
        "image/webp",
    ),
    Exact(b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
    Exact(b"\xFF\xD8\xFF", "image/jpeg"),
    masked(
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        b"FORM\x00\x00\x00\x00AIFF",
        "audio/aiff",
    ),
    masked(b"\xFF\xFF\xFF", b"ID3", "audio/mpeg"),
    masked(b"\xFF\xFF\xFF\xFF\xFF", b"OggS\x00", "application/ogg"),
    masked(
        b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
        b"MThd\x00\x00\x00\x06",
        "audio/midi",
    ),
    masked(
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00AVI ",
        "video/avi",
    ),
    masked(
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00WAVE",
        "audio/wave",
    ),
    Mp4,
    Exact(b"\x1A\x45\xDF\xA3", "video/webm"),
    masked(FONT_OBJECT_MASK, FONT_OBJECT_PATTERN, "application/vnd.ms-fontobject"),
    Exact(b"\x00\x01\x00\x00", "font/ttf"),
    Exact(b"OTTO", "font/otf"),
    Exact(b"ttcf", "font/collection"),
    Exact(b"wOFF", "font/woff"),
    Exact(b"wOF2", "font/woff2"),
    Exact(b"\x1F\x8B\x08", "application/x-gzip"),
    Exact(b"PK\x03\x04", "application/zip"),
    Exact(b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
    Exact(b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
    Exact(b"\x00\x61\x73\x6D", "application/wasm"),
    Text,
];

pub fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(SNIFF_LEN)];
    let first_non_whitespace = data.iter().position(|b| !is_whitespace(*b)).unwrap_or(data.len());
    SIGNATURES
        .iter()
        .find_map(|signature| signature.matches(data, first_non_whitespace))
        .unwrap_or(OCTET_STREAM)
}

fn is_whitespace(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

impl Signature {
    fn matches(&self, data: &[u8], first_non_whitespace: usize) -> Option<&'static str> {
        match self {
            Html(tag) => html_matches(tag, &data[first_non_whitespace..]).then_some(HTML),
            Exact(signature, content_type) => data.starts_with(signature).then_some(*content_type),
            Masked {
                mask,
                pattern,
                skip_whitespace,
                content_type,
            } => {
                let data = match skip_whitespace {
                    true => &data[first_non_whitespace..],
                    false => data,
                };
                masked_matches(mask, pattern, data).then_some(*content_type)
            }
            Mp4 => mp4_matches(data).then_some("video/mp4"),
            Text => text_matches(&data[first_non_whitespace..]).then_some(PLAIN),
        }
    }
}

fn html_matches(tag: &[u8], data: &[u8]) -> bool {
    data.len() > tag.len()
        && tag.iter().zip(data).all(|(expected, actual)| {
            let actual = match expected.is_ascii_uppercase() {
                true => actual & 0xDF,
                false => *actual,
            };
            *expected == actual
        })
        && matches!(data[tag.len()], b' ' | b'>')
}

fn masked_matches(mask: &[u8], pattern: &[u8], data: &[u8]) -> bool {
    pattern.len() == mask.len()
        && data.len() >= pattern.len()
        && pattern
            .iter()
            .zip(mask)
            .zip(data)
            .all(|((pattern, mask), byte)| byte & mask == *pattern)
}

fn mp4_matches(data: &[u8]) -> bool {
    let Some(size) = data.get(..4) else {
        return false;
    };
    let box_size = u32::from_be_bytes([size[0], size[1], size[2], size[3]]) as usize;
    data.len() >= 12
        && data.len() >= box_size
        && box_size.is_multiple_of(4)
        && &data[4..8] == b"ftyp"
        && (8..box_size)
            .step_by(4)
            .filter(|start| *start != 12)
            .any(|start| &data[start..start + 3] == b"mp4")
}

fn text_matches(data: &[u8]) -> bool {
    data.iter()
        .all(|b| !matches!(b, 0..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_like_go_net_http() {
        let cases: &[(&str, &[u8], &str)] = &[
            ("Empty", b"", "text/plain; charset=utf-8"),
            ("Binary", &[1, 2, 3], "application/octet-stream"),
            ("HTML document #1", b"<HtMl><bOdY>blah blah blah</body></html>", "text/html; charset=utf-8"),
            ("HTML document #2", b"<HTML></HTML>", "text/html; charset=utf-8"),
            ("HTML document #3", b"   <!DOCTYPE HTML>...", "text/html; charset=utf-8"),
            ("HTML document #4", b"\r\n<html>...", "text/html; charset=utf-8"),
            ("Plain text", "This is not HTML. It has \u{2603} though.".as_bytes(), "text/plain; charset=utf-8"),
            ("XML", b"\n<?xml!", "text/xml; charset=utf-8"),
            ("Windows icon", b"\x00\x00\x01\x00", "image/x-icon"),
            ("Windows cursor", b"\x00\x00\x02\x00", "image/x-icon"),
            ("BMP image", b"BM...", "image/bmp"),
            ("GIF 87a", b"GIF87a", "image/gif"),
            ("GIF 89a", b"GIF89a...", "image/gif"),
            ("WEBP image", b"RIFF\x00\x00\x00\x00WEBPVP", "image/webp"),
            ("PNG image", b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
            ("JPEG image", b"\xFF\xD8\xFF", "image/jpeg"),
            ("MIDI audio", b"MThd\x00\x00\x00\x06\x00\x01", "audio/midi"),
            ("MP3 audio", b"ID3\x03\x00\x00\x00\x00\x0f", "audio/mpeg"),
            ("WAV audio #1", b"RIFFb\xb8\x00\x00WAVEfmt \x12\x00\x00\x00\x06", "audio/wave"),
            ("WAV audio #2", b"RIFF,\x00\x00\x00WAVEfmt \x12\x00\x00\x00\x06", "audio/wave"),
            (
                "AIFF audio",
                b"FORM\x00\x00\x00\x00AIFFCOMM\x00\x00\x00\x12\x00\x01\x00\x00\x57\x55\x00\x10\x40\x0d\xf3\x34",
                "audio/aiff",
            ),
            (
                "OGG audio",
                b"OggS\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\x7e\x46\x00\x00\x00\x00\x00\x00\x1f\xf6\xb4\xfc\x01\x1e\x01\x76\x6f\x72",
                "application/ogg",
            ),
            ("Must not match OGG #1", b"owow\x00", "application/octet-stream"),
            ("Must not match OGG #2", b"oooS\x00", "application/octet-stream"),
            ("Must not match OGG #3", b"oggS\x00", "application/octet-stream"),
            ("MP4 video", b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom<\x06t\xbfmdat", "video/mp4"),
            ("AVI video #1", b"RIFF,O\n\x00AVI LIST\xc3\x80", "video/avi"),
            ("AVI video #2", b"RIFF,\n\x00\x00AVI LIST\xc3\x80", "video/avi"),
            ("TTF sample I", b"\x00\x01\x00\x00\x00\x17\x01\x00\x00\x04\x01\x60\x4f", "font/ttf"),
            ("TTF sample II", b"\x00\x01\x00\x00\x00\x0e\x00\x80\x00\x03\x00\x60\x46", "font/ttf"),
            ("OTTO sample", b"\x4f\x54\x54\x4f\x00\x0e\x00\x80\x00\x03\x00\x60\x42\x41\x53\x45", "font/otf"),
            ("woff sample", b"\x77\x4f\x46\x46\x00\x01\x00\x00\x00\x00\x30\x54\x00\x0d\x00\x00", "font/woff"),
            ("woff2 sample", b"\x77\x4f\x46\x32\x00\x01\x00\x00\x00", "font/woff2"),
            ("wasm sample", b"\x00\x61\x73\x6d\x01\x00", "application/wasm"),
            ("RAR v1.5-v4.0", b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
            ("RAR v5+", b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
            ("Incorrect RAR v1.5-v4.0", b"Rar \x1A\x07\x00", "application/octet-stream"),
            ("Incorrect RAR v5+", b"Rar \x1A\x07\x01\x00", "application/octet-stream"),
            ("PDF", b"%PDF-1.4", "application/pdf"),
            ("PostScript", b"%!PS-Adobe-3.0", "application/postscript"),
            ("UTF-8 BOM", b"\xEF\xBB\xBFhello", "text/plain; charset=utf-8"),
            ("UTF-16BE BOM", b"\xFE\xFF\x00a", "text/plain; charset=utf-16be"),
            ("UTF-16LE BOM", b"\xFF\xFE\x00a", "text/plain; charset=utf-16le"),
            ("gzip", b"\x1F\x8B\x08\x00", "application/x-gzip"),
            ("zip", b"PK\x03\x04", "application/zip"),
            ("webm", b"\x1A\x45\xDF\xA3", "video/webm"),
            ("woff in the middle of the table", b"wOFF", "font/woff"),
        ];
        for (description, data, expected) in cases {
            assert_eq!(detect_content_type(data), *expected, "{description}");
        }
    }

    #[test]
    fn considers_at_most_the_first_512_bytes() {
        let mut data = vec![b'a'; 512];
        data.push(0x01);
        assert_eq!(detect_content_type(&data), "text/plain; charset=utf-8");
    }

    #[test]
    fn html_needs_a_tag_terminating_byte() {
        assert_eq!(detect_content_type(b"<html"), "text/plain; charset=utf-8");
        assert_eq!(detect_content_type(b"<html>"), "text/html; charset=utf-8");
        assert_eq!(detect_content_type(b"<htmlx>"), "text/plain; charset=utf-8");
    }
}
