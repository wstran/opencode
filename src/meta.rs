//! Metadata readers for the non-text files ocode opens.
//!
//! Everything here parses untrusted bytes, so every read goes through the
//! checked helpers on [`Bytes`] and every walk is bounded. A truncated or
//! deliberately malformed file must produce fewer fields, never a panic: the
//! whole point of this view is to inspect files you do not trust yet.
//!
//! Nothing writes. Reading metadata never rewrites the file, so opening a photo
//! here cannot strip its EXIF the way some viewers do on save.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// A titled block of `label: value` rows.
pub struct Group {
    pub title: String,

    pub fields: Vec<(String, String)>,
}

impl Group {
    fn new(title: impl Into<String>) -> Self {
        Self { title: title.into(), fields: Vec::new() }
    }

    fn put(&mut self, label: impl Into<String>, value: impl Into<String>) {
        self.fields.push((label.into(), value.into()));
    }

    fn put_opt(&mut self, label: impl Into<String>, value: Option<String>) {
        if let Some(v) = value {
            if !v.trim().is_empty() {
                self.put(label, v.trim().to_string());
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Checked reader over a byte slice. Every accessor returns `None` rather than
/// panicking when the file is shorter than its own headers claim.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn slice(&self, at: usize, len: usize) -> Option<&'a [u8]> {
        self.0.get(at..at.checked_add(len)?)
    }

    fn u8(&self, at: usize) -> Option<u8> {
        self.0.get(at).copied()
    }

    fn u16(&self, at: usize, big: bool) -> Option<u16> {
        let b = self.slice(at, 2)?;

        Some(if big {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    }

    fn u32(&self, at: usize, big: bool) -> Option<u32> {
        let b = self.slice(at, 4)?;

        Some(if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    }

    fn u64(&self, at: usize, big: bool) -> Option<u64> {
        let b = self.slice(at, 8)?;

        let mut v = [0u8; 8];

        v.copy_from_slice(b);

        Some(if big { u64::from_be_bytes(v) } else { u64::from_le_bytes(v) })
    }
}

/// Printable form of a byte run, for text embedded in binary containers.
fn text(bytes: &[u8]) -> String {
    let cut = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());

    String::from_utf8_lossy(&bytes[..cut])
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect()
}

/// A four-character type code. Padding spaces are real (QuickTime's brand is
/// `qt  `), so they survive and only genuinely unprintable bytes become dots.
fn four_cc(bytes: &[u8]) -> String {
    let s: String = bytes
        .iter()
        .map(|b| if b.is_ascii_graphic() || *b == b' ' { *b as char } else { '.' })
        .collect();

    s.trim_end().to_string()
}

/// PDF text strings are either PDFDocEncoding or UTF-16BE behind a byte-order
/// mark. Decoding the second as bytes yields a row of replacement characters,
/// which is worse than showing nothing.
fn pdf_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]))
            .collect();

        let decoded: String = String::from_utf16_lossy(&units)
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();

        return (!decoded.trim().is_empty()).then_some(decoded);
    }

    let plain = text(bytes);

    // Anything that decoded mostly to replacement characters is some other
    // encoding this reader does not claim to understand.
    let bad = plain.chars().filter(|c| *c == char::REPLACEMENT_CHARACTER).count();

    (bad * 4 < plain.chars().count().max(1)).then_some(plain)
}

/// Read a bounded region, for formats whose metadata is not in the prefix the
/// classifier already holds.
fn read_at(path: &Path, offset: u64, len: usize) -> Option<Vec<u8>> {
    const CAP: usize = 1 << 20;

    let mut file = fs::File::open(path).ok()?;

    file.seek(SeekFrom::Start(offset)).ok()?;

    let mut buf = vec![0u8; len.min(CAP)];

    let mut filled = 0;

    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,

            Ok(n) => filled += n,

            Err(_) => break,
        }
    }

    buf.truncate(filled);

    Some(buf)
}

/// Everything known about `path`, given the prefix the classifier already read.
pub fn describe(path: &Path, head: &[u8], byte_len: u64) -> Vec<Group> {
    let b = Bytes(head);

    let mut groups = Vec::new();

    let mut file = Group::new("File");

    file.put("Name", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());

    file.put("Size", format!("{} ({} bytes)", crate::media::human_size(byte_len), byte_len));

    groups.push(file);

    let specific = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(&b)
    } else if head.starts_with(&[0xff, 0xd8]) {
        jpeg(&b)
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        gif(&b)
    } else if head.starts_with(b"BM") {
        bmp(&b)
    } else if head.starts_with(b"RIFF") && b.slice(8, 4) == Some(b"WEBP") {
        webp(&b)
    } else if b.slice(4, 4) == Some(b"ftyp") {
        mp4(path, &b, byte_len)
    } else if head.starts_with(b"%PDF") {
        pdf(&b)
    } else if head.starts_with(b"PK\x03\x04") {
        zip(path, byte_len)
    } else if head.starts_with(&[0x1f, 0x8b]) {
        gzip(&b)
    } else if head.starts_with(b"\x7fELF") {
        elf(&b)
    } else if head.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) || head.starts_with(&[0xce, 0xfa, 0xed, 0xfe]) {
        macho(&b)
    } else if head.starts_with(b"\0asm") {
        wasm(&b)
    } else if head.starts_with(b"ID3") {
        id3(&b)
    } else if head.starts_with(b"fLaC") {
        flac(&b)
    } else if head.starts_with(b"SQLite format 3\0") {
        sqlite(&b)
    } else if head.starts_with(&[0x00, 0x01, 0x00, 0x00]) || head.starts_with(b"OTTO") || head.starts_with(b"true") {
        font(&b)
    } else {
        Vec::new()
    };

    groups.extend(specific.into_iter().filter(|g| !g.is_empty()));

    groups
}

// ---------------------------------------------------------------- images

fn png(b: &Bytes) -> Vec<Group> {
    let mut image = Group::new("PNG");

    let mut text_group = Group::new("Text chunks");

    let mut chunks: Vec<String> = Vec::new();

    let mut at = 8;

    // Bounded walk: a corrupt length field must not spin here.
    for _ in 0..512 {
        let Some(len) = b.u32(at, true) else { break };

        let Some(kind) = b.slice(at + 4, 4) else { break };

        let name = four_cc(kind);

        let data_at = at + 8;

        let len = len as usize;

        match kind {
            b"IHDR" => {
                if let (Some(w), Some(h)) = (b.u32(data_at, true), b.u32(data_at + 4, true)) {
                    image.put("Dimensions", format!("{w} x {h} px"));
                }

                if let Some(depth) = b.u8(data_at + 8) {
                    image.put("Bit depth", depth.to_string());
                }

                if let Some(color) = b.u8(data_at + 9) {
                    image.put("Colour type", png_colour(color));
                }

                if let Some(interlace) = b.u8(data_at + 12) {
                    image.put("Interlace", if interlace == 0 { "none" } else { "Adam7" });
                }
            }

            b"pHYs" => {
                if let (Some(x), Some(y), Some(unit)) =
                    (b.u32(data_at, true), b.u32(data_at + 4, true), b.u8(data_at + 8))
                {
                    if unit == 1 {
                        // Stored per metre; DPI is the form people recognise.
                        let dpi = |v: u32| (v as f64 * 0.0254).round() as u64;

                        image.put("Resolution", format!("{} x {} dpi", dpi(x), dpi(y)));
                    } else {
                        image.put("Aspect", format!("{x}:{y}"));
                    }
                }
            }

            b"gAMA" => {
                if let Some(g) = b.u32(data_at, true) {
                    image.put("Gamma", format!("{:.5}", g as f64 / 100_000.0));
                }
            }

            b"tIME" => {
                if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (
                    b.u16(data_at, true),
                    b.u8(data_at + 2),
                    b.u8(data_at + 3),
                    b.u8(data_at + 4),
                    b.u8(data_at + 5),
                    b.u8(data_at + 6),
                ) {
                    image.put("Modified", format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"));
                }
            }

            b"acTL" => {
                if let Some(frames) = b.u32(data_at, true) {
                    image.put("Animated", format!("{frames} frames"));
                }
            }

            b"iCCP" => image.put("Colour profile", "embedded ICC"),

            b"tEXt" | b"iTXt" | b"zTXt" => {
                if let Some(data) = b.slice(data_at, len) {
                    let split = data.iter().position(|c| *c == 0).unwrap_or(data.len());

                    let key = text(&data[..split]);

                    // zTXt/iTXt payloads may be deflated; show the key alone
                    // rather than a run of binary.
                    let value = if kind == b"tEXt" {
                        text(data.get(split + 1..).unwrap_or_default())
                    } else {
                        String::new()
                    };

                    if !key.is_empty() {
                        text_group.put(key, if value.is_empty() { "(compressed)".to_string() } else { value });
                    }
                }
            }

            _ => {}
        }

        if !chunks.contains(&name) {
            chunks.push(name);
        }

        if kind == b"IEND" {
            break;
        }

        // length + type + data + CRC
        let Some(next) = at.checked_add(12).and_then(|n| n.checked_add(len)) else { break };

        at = next;
    }

    if !chunks.is_empty() {
        image.put("Chunks", chunks.join(" "));
    }

    vec![image, text_group]
}

fn png_colour(v: u8) -> &'static str {
    match v {
        0 => "greyscale",

        2 => "truecolour",

        3 => "indexed",

        4 => "greyscale + alpha",

        6 => "truecolour + alpha",

        _ => "unknown",
    }
}

fn jpeg(b: &Bytes) -> Vec<Group> {
    let mut image = Group::new("JPEG");

    let mut comment = Group::new("Comment");

    let mut exif_groups = Vec::new();

    let mut at = 2;

    for _ in 0..256 {
        if b.u8(at) != Some(0xff) {
            break;
        }

        let Some(marker) = b.u8(at + 1) else { break };

        // Standalone markers carry no length.
        if (0xd0..=0xd9).contains(&marker) {
            at += 2;

            continue;
        }

        let Some(len) = b.u16(at + 2, true) else { break };

        let len = len as usize;

        let data_at = at + 4;

        let data_len = len.saturating_sub(2);

        match marker {
            // SOF0..SOF15 except the DHT/JPG/DAC markers interleaved in range.
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                if let (Some(prec), Some(h), Some(w), Some(comps)) = (
                    b.u8(data_at),
                    b.u16(data_at + 1, true),
                    b.u16(data_at + 3, true),
                    b.u8(data_at + 5),
                ) {
                    image.put("Dimensions", format!("{w} x {h} px"));

                    image.put("Precision", format!("{prec}-bit"));

                    image.put(
                        "Components",
                        match comps {
                            1 => "1 (greyscale)".to_string(),

                            3 => "3 (YCbCr)".to_string(),

                            4 => "4 (CMYK)".to_string(),

                            n => n.to_string(),
                        },
                    );

                    image.put("Encoding", if marker == 0xc2 { "progressive" } else { "baseline" });
                }
            }

            // APP0: JFIF density.
            0xe0 if b.slice(data_at, 5) == Some(b"JFIF\0") => {
                {
                    if let (Some(unit), Some(x), Some(y)) =
                        (b.u8(data_at + 7), b.u16(data_at + 8, true), b.u16(data_at + 10, true))
                    {
                        match unit {
                            1 => image.put("Density", format!("{x} x {y} dpi")),

                            2 => image.put("Density", format!("{x} x {y} dots/cm")),

                            // Unit 0 carries no physical size, only a pixel
                            // aspect ratio, which is worth saying only when the
                            // pixels are not square.
                            _ if x != y => image.put("Pixel aspect", format!("{x}:{y}")),

                            _ => {}
                        }
                    }
                }
            }

            // APP1: EXIF.
            0xe1 if b.slice(data_at, 6) == Some(b"Exif\0\0") => {
                {
                    if let Some(tiff) = b.slice(data_at + 6, data_len.saturating_sub(6)) {
                        exif_groups = exif(tiff);
                    }
                }
            }

            0xfe => {
                if let Some(data) = b.slice(data_at, data_len) {
                    comment.put_opt("Text", Some(text(data)));
                }
            }

            // Start of scan: entropy-coded data follows, stop walking.
            0xda => break,

            _ => {}
        }

        let Some(next) = at.checked_add(2).and_then(|n| n.checked_add(len)) else { break };

        at = next;
    }

    let mut out = vec![image, comment];

    out.extend(exif_groups);

    out
}

// ------------------------------------------------------------------ EXIF

/// EXIF lives in a TIFF structure: a header naming the byte order, then a chain
/// of IFDs of 12-byte entries. Walked by hand and deliberately conservatively:
/// only a curated set of tags is decoded, anything unexpected is skipped.
fn exif(tiff: &[u8]) -> Vec<Group> {
    let b = Bytes(tiff);

    let big = match b.slice(0, 2) {
        Some(b"MM") => true,

        Some(b"II") => false,

        _ => return Vec::new(),
    };

    if b.u16(2, big) != Some(42) {
        return Vec::new();
    }

    let Some(ifd0) = b.u32(4, big) else { return Vec::new() };

    let mut camera = Group::new("Camera");

    let mut shot = Group::new("Exposure");

    let mut gps = Group::new("Location");

    let mut exif_ifd = None;

    let mut gps_ifd = None;

    for (tag, value) in ifd_entries(&b, ifd0 as usize, big) {
        match tag {
            0x8769 => exif_ifd = value.as_u32(),

            0x8825 => gps_ifd = value.as_u32(),

            0x010f => camera.put_opt("Make", value.as_text()),

            0x0110 => camera.put_opt("Model", value.as_text()),

            0x0131 => camera.put_opt("Software", value.as_text()),

            0x013b => camera.put_opt("Artist", value.as_text()),

            0x8298 => camera.put_opt("Copyright", value.as_text()),

            0x0132 => shot.put_opt("Date", value.as_text().map(exif_date)),

            0x0112 => shot.put_opt("Orientation", value.as_u32().map(orientation)),

            _ => {}
        }
    }

    if let Some(off) = exif_ifd {
        for (tag, value) in ifd_entries(&b, off as usize, big) {
            match tag {
                0x9003 => shot.put_opt("Taken", value.as_text().map(exif_date)),

                0x829a => shot.put_opt("Shutter", value.as_ratio().map(shutter)),

                0x829d => shot.put_opt("Aperture", value.as_ratio().map(|r| format!("f/{r:.1}"))),

                0x8827 => shot.put_opt("ISO", value.as_u32().map(|v| v.to_string())),

                0x920a => shot.put_opt("Focal length", value.as_ratio().map(|r| format!("{r:.0} mm"))),

                0xa002 => camera.put_opt("Pixel width", value.as_u32().map(|v| v.to_string())),

                0xa003 => camera.put_opt("Pixel height", value.as_u32().map(|v| v.to_string())),

                0xa434 => camera.put_opt("Lens", value.as_text()),

                0x9209 => shot.put_opt("Flash", value.as_u32().map(|v| {
                    if v & 1 == 1 { "fired".to_string() } else { "did not fire".to_string() }
                })),

                _ => {}
            }
        }
    }

    if let Some(off) = gps_ifd {
        let mut lat = None;

        let mut lat_ref = None;

        let mut lon = None;

        let mut lon_ref = None;

        for (tag, value) in ifd_entries(&b, off as usize, big) {
            match tag {
                0x0001 => lat_ref = value.as_text(),

                0x0002 => lat = value.as_dms(),

                0x0003 => lon_ref = value.as_text(),

                0x0004 => lon = value.as_dms(),

                0x0006 => gps.put_opt("Altitude", value.as_ratio().map(|r| format!("{r:.0} m"))),

                _ => {}
            }
        }

        if let (Some(lat), Some(lon)) = (lat, lon) {
            let sign = |r: Option<String>, neg: &str| {
                if r.as_deref().map(|s| s.starts_with(neg)).unwrap_or(false) { -1.0 } else { 1.0 }
            };

            gps.put(
                "Coordinates",
                format!("{:.6}, {:.6}", lat * sign(lat_ref, "S"), lon * sign(lon_ref, "W")),
            );

            gps.put("Note", "this file records where it was taken");
        }
    }

    vec![camera, shot, gps]
}

/// One decoded IFD entry payload.
struct Value<'a> {
    kind: u16,

    count: u32,

    raw: &'a [u8],

    big: bool,
}

impl Value<'_> {
    fn as_text(&self) -> Option<String> {
        (self.kind == 2).then(|| text(self.raw))
    }

    fn as_u32(&self) -> Option<u32> {
        let b = Bytes(self.raw);

        match self.kind {
            3 => b.u16(0, self.big).map(u32::from),

            4 | 9 => b.u32(0, self.big),

            1 => b.u8(0).map(u32::from),

            _ => None,
        }
    }

    fn as_ratio(&self) -> Option<f64> {
        let b = Bytes(self.raw);

        if self.kind != 5 && self.kind != 10 {
            return None;
        }

        let num = b.u32(0, self.big)? as f64;

        let den = b.u32(4, self.big)? as f64;

        (den != 0.0).then(|| num / den)
    }

    /// Degrees/minutes/seconds triple, as used by the GPS tags.
    fn as_dms(&self) -> Option<f64> {
        if self.kind != 5 || self.count < 3 {
            return None;
        }

        let b = Bytes(self.raw);

        let part = |i: usize| -> Option<f64> {
            let num = b.u32(i * 8, self.big)? as f64;

            let den = b.u32(i * 8 + 4, self.big)? as f64;

            (den != 0.0).then(|| num / den)
        };

        Some(part(0)? + part(1)? / 60.0 + part(2)? / 3600.0)
    }
}

fn ifd_entries<'a>(b: &Bytes<'a>, at: usize, big: bool) -> Vec<(u16, Value<'a>)> {
    let mut out = Vec::new();

    let Some(count) = b.u16(at, big) else { return out };

    for i in 0..count.min(512) as usize {
        let entry = at + 2 + i * 12;

        let (Some(tag), Some(kind), Some(n)) =
            (b.u16(entry, big), b.u16(entry + 2, big), b.u32(entry + 4, big))
        else {
            break;
        };

        let unit = match kind {
            1 | 2 | 6 | 7 => 1,

            3 | 8 => 2,

            4 | 9 | 11 => 4,

            5 | 10 | 12 => 8,

            _ => 0,
        };

        if unit == 0 {
            continue;
        }

        let bytes = (n as usize).saturating_mul(unit);

        // Up to four bytes live in the entry itself, anything longer is at an
        // offset from the start of the TIFF block.
        let raw = if bytes <= 4 {
            b.slice(entry + 8, bytes)
        } else {
            b.u32(entry + 8, big).and_then(|off| b.slice(off as usize, bytes))
        };

        if let Some(raw) = raw {
            out.push((tag, Value { kind, count: n, raw, big }));
        }
    }

    out
}

fn orientation(v: u32) -> String {
    match v {
        1 => "normal",

        2 => "mirrored",

        3 => "rotated 180°",

        4 => "mirrored, rotated 180°",

        5 => "mirrored, rotated 90° CCW",

        6 => "rotated 90° CW",

        7 => "mirrored, rotated 90° CW",

        8 => "rotated 90° CCW",

        _ => "unknown",
    }
    .to_string()
}

/// EXIF writes timestamps as `YYYY:MM:DD hh:mm:ss`; the colons in the date read
/// as a typo to everyone except EXIF.
fn exif_date(raw: String) -> String {
    let mut chars: Vec<char> = raw.chars().collect();

    if chars.len() >= 10 && chars[4] == ':' && chars[7] == ':' {
        chars[4] = '-';

        chars[7] = '-';
    }

    chars.into_iter().collect()
}

fn shutter(seconds: f64) -> String {
    if seconds >= 1.0 {
        format!("{seconds:.1} s")
    } else if seconds > 0.0 {
        format!("1/{:.0} s", 1.0 / seconds)
    } else {
        "0 s".to_string()
    }
}

// ------------------------------------------------------- other containers

fn gif(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("GIF");

    g.put_opt("Version", b.slice(3, 3).map(text));

    if let (Some(w), Some(h)) = (b.u16(6, false), b.u16(8, false)) {
        g.put("Dimensions", format!("{w} x {h} px"));
    }

    if let Some(flags) = b.u8(10) {
        let bits = (flags & 0b111) + 1;

        g.put("Colour table", format!("{} colours", 1u32 << bits));
    }

    // NETSCAPE2.0 is what makes a GIF loop.
    if b.0.windows(11).any(|w| w == b"NETSCAPE2.0") {
        g.put("Animation", "looping");
    }

    vec![g]
}

fn bmp(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("BMP");

    if let (Some(w), Some(h)) = (b.u32(18, false), b.u32(22, false)) {
        g.put("Dimensions", format!("{w} x {} px", h as i32));
    }

    if let Some(bpp) = b.u16(28, false) {
        g.put("Bit depth", format!("{bpp}-bit"));
    }

    vec![g]
}

fn webp(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("WebP");

    let kind = b.slice(12, 4).map(four_cc).unwrap_or_default();

    g.put(
        "Encoding",
        match kind.as_str() {
            "VP8 " => "lossy",

            "VP8L" => "lossless",

            "VP8X" => "extended",

            _ => "unknown",
        },
    );

    if kind == "VP8X" {
        // 24-bit little-endian, stored minus one.
        let dim = |at: usize| -> Option<u32> {
            Some(u32::from(b.u8(at)?) | u32::from(b.u8(at + 1)?) << 8 | u32::from(b.u8(at + 2)?) << 16)
        };

        if let (Some(w), Some(h)) = (dim(24), dim(27)) {
            g.put("Dimensions", format!("{} x {} px", w + 1, h + 1));
        }

        if let Some(flags) = b.u8(20) {
            let mut on = Vec::new();

            if flags & 0b0000_0010 != 0 {
                on.push("animation");
            }

            if flags & 0b0001_0000 != 0 {
                on.push("alpha");
            }

            if flags & 0b0000_1000 != 0 {
                on.push("EXIF");
            }

            if flags & 0b0000_0100 != 0 {
                on.push("XMP");
            }

            if !on.is_empty() {
                g.put("Features", on.join(", "));
            }
        }
    }

    vec![g]
}

/// MP4 and QuickTime. The `moov` box holding the interesting fields is often at
/// the end of the file, so top-level boxes are walked through the file rather
/// than only the prefix.
fn mp4(path: &Path, head: &Bytes, byte_len: u64) -> Vec<Group> {
    let mut g = Group::new("Video");

    g.put_opt("Brand", head.slice(8, 4).map(four_cc));

    let mut at = 0u64;

    let mut moov = None;

    for _ in 0..64 {
        let Some(buf) = read_at(path, at, 16) else { break };

        let b = Bytes(&buf);

        let (Some(size), Some(kind)) = (b.u32(0, true), b.slice(4, 4)) else { break };

        let name = four_cc(kind);

        let size = match size {
            // 1 means the real size is a 64-bit field after the type.
            1 => b.u64(8, true).unwrap_or(0),

            0 => byte_len.saturating_sub(at),

            n => u64::from(n),
        };

        if size < 8 {
            break;
        }

        if name == "moov" {
            moov = Some((at, size));

            break;
        }

        let Some(next) = at.checked_add(size) else { break };

        if next >= byte_len {
            break;
        }

        at = next;
    }

    if let Some((off, size)) = moov {
        // Cap the read: a moov box can be large, the header fields are early.
        if let Some(buf) = read_at(path, off, size.min(1 << 16) as usize) {
            let b = Bytes(&buf);

            if let Some(mvhd) = find_box(&b, b"mvhd") {
                let version = b.u8(mvhd + 8).unwrap_or(0);

                let (timescale, duration) = if version == 1 {
                    (b.u32(mvhd + 28, true), b.u64(mvhd + 32, true))
                } else {
                    (b.u32(mvhd + 20, true), b.u32(mvhd + 24, true).map(u64::from))
                };

                if let (Some(scale), Some(dur)) = (timescale, duration) {
                    if scale > 0 {
                        let secs = dur as f64 / f64::from(scale);

                        g.put("Duration", duration_text(secs));
                    }
                }

                // Seconds since 1904-01-01, the QuickTime epoch.
                let created = if version == 1 { b.u64(mvhd + 12, true) } else { b.u32(mvhd + 12, true).map(u64::from) };

                if let Some(c) = created.filter(|c| *c > 0) {
                    g.put_opt("Created", mac_time(c));
                }
            }

            if let Some(tkhd) = find_box(&b, b"tkhd") {
                let version = b.u8(tkhd + 8).unwrap_or(0);

                // From the box start: 8 header + 4 version/flags, then the
                // timestamps and ids, 8 reserved, layer/group/volume/reserved,
                // and a 36-byte matrix before the two fixed-point dimensions.
                let base = if version == 1 { tkhd + 96 } else { tkhd + 84 };

                // 16.16 fixed point.
                if let (Some(w), Some(h)) = (b.u32(base, true), b.u32(base + 4, true)) {
                    if w > 0 && h > 0 {
                        g.put("Dimensions", format!("{} x {} px", w >> 16, h >> 16));
                    }
                }
            }

            let mut codecs: Vec<String> = Vec::new();

            for tag in [&b"avc1"[..], b"hvc1", b"hev1", b"mp4a", b"av01", b"vp09"] {
                if find_box(&b, tag).is_some() {
                    codecs.push(four_cc(tag));
                }
            }

            if !codecs.is_empty() {
                g.put("Codecs", codecs.join(", "));
            }
        }
    }

    vec![g]
}

/// Locate a four-character box type anywhere in a buffer. Boxes are nested, and
/// a scan is enough for the handful of headers read here.
fn find_box(b: &Bytes, kind: &[u8]) -> Option<usize> {
    b.0.windows(4).position(|w| w == kind).map(|p| p.saturating_sub(4))
}

fn duration_text(secs: f64) -> String {
    let total = secs.round() as u64;

    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);

    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// QuickTime timestamps count from 1904; convert to the Unix epoch and format.
fn mac_time(secs: u64) -> Option<String> {
    const EPOCH_DELTA: u64 = 2_082_844_800;

    let unix = secs.checked_sub(EPOCH_DELTA)?;

    Some(civil_from_unix(unix))
}

/// Days-since-epoch to a calendar date, so no date dependency is needed.
fn civil_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;

    let rem = secs % 86_400;

    let z = days + 719_468;

    let era = z / 146_097;

    let doe = z - era * 146_097;

    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;

    let y = yoe + era * 400;

    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);

    let mp = (5 * doy + 2) / 153;

    let d = doy - (153 * mp + 2) / 5 + 1;

    let m = if mp < 10 { mp + 3 } else { mp - 9 };

    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn pdf(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("PDF");

    g.put_opt("Version", b.slice(5, 3).map(text));

    if b.0.windows(9).any(|w| w == b"/Encrypt ") {
        g.put("Encrypted", "yes");
    }

    for (needle, label) in [
        (&b"/Title ("[..], "Title"),
        (&b"/Author ("[..], "Author"),
        (&b"/Producer ("[..], "Producer"),
        (&b"/Creator ("[..], "Creator"),
    ] {
        if let Some(pos) = b.0.windows(needle.len()).position(|w| w == needle) {
            let start = pos + needle.len();

            let end = b.0[start..].iter().position(|c| *c == b')').unwrap_or(0);

            if end > 0 {
                g.put_opt(label, b.slice(start, end).and_then(pdf_text));
            }
        }
    }

    vec![g]
}

fn zip(path: &Path, byte_len: u64) -> Vec<Group> {
    let mut g = Group::new("Archive");

    // The end-of-central-directory record sits in the last 64KB.
    let tail_len = byte_len.min(66_000) as usize;

    let start = byte_len.saturating_sub(tail_len as u64);

    if let Some(buf) = read_at(path, start, tail_len) {
        let b = Bytes(&buf);

        if let Some(pos) = buf.windows(4).rposition(|w| w == b"PK\x05\x06") {
            if let Some(entries) = b.u16(pos + 10, false) {
                g.put("Entries", entries.to_string());
            }

            if let Some(size) = b.u32(pos + 12, false) {
                g.put("Directory", crate::media::human_size(u64::from(size)));
            }

            if let Some(len) = b.u16(pos + 20, false).filter(|l| *l > 0) {
                g.put_opt("Comment", b.slice(pos + 22, len as usize).map(text));
            }
        }
    }

    vec![g]
}

fn gzip(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("gzip");

    if let Some(flags) = b.u8(3) {
        // FNAME: the original file name follows the 10-byte header.
        if flags & 0b0000_1000 != 0 {
            g.put_opt("Original name", b.slice(10, 100).map(text));
        }
    }

    if let Some(mtime) = b.u32(4, false).filter(|t| *t > 0) {
        g.put("Modified", civil_from_unix(u64::from(mtime)));
    }

    vec![g]
}

fn elf(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("ELF");

    g.put("Class", if b.u8(4) == Some(2) { "64-bit" } else { "32-bit" });

    let big = b.u8(5) == Some(2);

    g.put("Byte order", if big { "big endian" } else { "little endian" });

    g.put_opt(
        "Type",
        b.u16(16, big).map(|t| match t {
            1 => "relocatable".to_string(),

            2 => "executable".to_string(),

            3 => "shared object".to_string(),

            4 => "core dump".to_string(),

            n => format!("type {n}"),
        }),
    );

    g.put_opt(
        "Machine",
        b.u16(18, big).map(|m| match m {
            0x03 => "x86".to_string(),

            0x28 => "ARM".to_string(),

            0x3e => "x86-64".to_string(),

            0xb7 => "AArch64".to_string(),

            0xf3 => "RISC-V".to_string(),

            n => format!("machine {n:#x}"),
        }),
    );

    vec![g]
}

fn macho(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("Mach-O");

    let sixty_four = b.slice(0, 4) == Some(&[0xcf, 0xfa, 0xed, 0xfe]);

    g.put("Class", if sixty_four { "64-bit" } else { "32-bit" });

    g.put_opt(
        "Architecture",
        b.u32(4, false).map(|c| match c & 0x00ff_ffff {
            7 => "x86".to_string(),

            12 => "ARM".to_string(),

            n => format!("cpu {n}"),
        }),
    );

    g.put_opt(
        "Type",
        b.u32(12, false).map(|t| match t {
            1 => "object".to_string(),

            2 => "executable".to_string(),

            6 => "dynamic library".to_string(),

            8 => "bundle".to_string(),

            n => format!("type {n}"),
        }),
    );

    vec![g]
}

fn wasm(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("WebAssembly");

    g.put_opt("Version", b.u32(4, false).map(|v| v.to_string()));

    let names = [
        "custom", "type", "import", "function", "table", "memory", "global", "export", "start",
        "element", "code", "data", "data count",
    ];

    let mut seen: Vec<&str> = Vec::new();

    let mut at = 8;

    for _ in 0..64 {
        let Some(id) = b.u8(at) else { break };

        let Some((len, used)) = uleb(b, at + 1) else { break };

        if let Some(name) = names.get(id as usize) {
            if !seen.contains(name) {
                seen.push(name);
            }
        }

        let Some(next) = at.checked_add(1 + used).and_then(|n| n.checked_add(len as usize)) else { break };

        at = next;
    }

    if !seen.is_empty() {
        g.put("Sections", seen.join(" "));
    }

    vec![g]
}

/// LEB128, as wasm sizes are encoded. Returns the value and the bytes consumed.
fn uleb(b: &Bytes, at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;

    let mut shift = 0;

    for i in 0..10 {
        let byte = b.u8(at + i)?;

        value |= u64::from(byte & 0x7f) << shift;

        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }

        shift += 7;
    }

    None
}

fn id3(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("Audio tags");

    let major = b.u8(3).unwrap_or(0);

    g.put("Tag", format!("ID3v2.{major}"));

    // Header size is a 28-bit synchsafe integer: seven bits per byte.
    let parts: Vec<u32> = (6..10).filter_map(|i| b.u8(i).map(u32::from)).collect();

    if parts.len() != 4 {
        return vec![g];
    }

    let size = (parts[0] << 21) | (parts[1] << 14) | (parts[2] << 7) | parts[3];

    let mut at = 10usize;

    let end = (10 + size as usize).min(b.0.len());

    while at + 10 <= end {
        let Some(id) = b.slice(at, 4) else { break };

        if id == [0, 0, 0, 0] {
            break;
        }

        let Some(len) = b.u32(at + 4, true) else { break };

        let len = len as usize;

        let label = match id {
            b"TIT2" => Some("Title"),

            b"TPE1" => Some("Artist"),

            b"TALB" => Some("Album"),

            b"TYER" | b"TDRC" => Some("Year"),

            b"TCON" => Some("Genre"),

            b"TRCK" => Some("Track"),

            _ => None,
        };

        if let Some(label) = label {
            // First byte is the text encoding, skip it.
            if let Some(data) = b.slice(at + 11, len.saturating_sub(1)) {
                g.put_opt(label, Some(text(data)));
            }
        }

        let Some(next) = at.checked_add(10).and_then(|n| n.checked_add(len)) else { break };

        at = next;
    }

    vec![g]
}

fn flac(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("FLAC");

    // STREAMINFO is always the first metadata block, at offset 8.
    let rate = b
        .u32(18, true)
        .map(|v| v >> 12)
        .filter(|r| *r > 0);

    if let Some(rate) = rate {
        g.put("Sample rate", format!("{rate} Hz"));

        if let (Some(hi), Some(lo)) = (b.u8(21), b.u32(22, true)) {
            let samples = (u64::from(hi & 0x0f) << 32) | u64::from(lo);

            if samples > 0 {
                g.put("Duration", duration_text(samples as f64 / f64::from(rate)));
            }
        }
    }

    if let Some(byte) = b.u8(20) {
        g.put("Channels", (((byte >> 1) & 0b111) + 1).to_string());
    }

    vec![g]
}

fn sqlite(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("SQLite");

    if let Some(page) = b.u16(16, true) {
        // 1 encodes 65536.
        let size = if page == 1 { 65_536 } else { u32::from(page) };

        g.put("Page size", format!("{size} bytes"));
    }

    if let Some(count) = b.u32(28, true) {
        g.put("Pages", count.to_string());
    }

    if let Some(v) = b.u32(96, true) {
        g.put("Written by", format!("SQLite {}.{}.{}", v / 1_000_000, (v / 1000) % 1000, v % 1000));
    }

    vec![g]
}

fn font(b: &Bytes) -> Vec<Group> {
    let mut g = Group::new("Font");

    g.put("Flavour", if b.slice(0, 4) == Some(b"OTTO") { "OpenType (CFF)" } else { "TrueType" });

    let Some(tables) = b.u16(4, true) else { return vec![g] };

    g.put("Tables", tables.to_string());

    // Find the name table, then pull the family and subfamily records.
    for i in 0..tables.min(256) as usize {
        let rec = 12 + i * 16;

        if b.slice(rec, 4) != Some(b"name") {
            continue;
        }

        let Some(off) = b.u32(rec + 8, true).map(|v| v as usize) else { break };

        let (Some(count), Some(strings)) = (b.u16(off + 2, true), b.u16(off + 4, true)) else { break };

        for j in 0..count.min(256) as usize {
            let entry = off + 6 + j * 12;

            let (Some(name_id), Some(len), Some(str_off)) =
                (b.u16(entry + 6, true), b.u16(entry + 8, true), b.u16(entry + 10, true))
            else {
                break;
            };

            let label = match name_id {
                1 => "Family",

                2 => "Style",

                5 => "Version",

                _ => continue,
            };

            let at = off + strings as usize + str_off as usize;

            if let Some(raw) = b.slice(at, len as usize) {
                // Most records are UTF-16BE; drop the high bytes for ASCII names.
                let ascii: Vec<u8> = raw.iter().copied().filter(|c| *c != 0).collect();

                g.put_opt(label, Some(text(&ascii)));
            }
        }

        break;
    }

    vec![g]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_fixture() -> Vec<u8> {
        let mut img = image::RgbaImage::new(7, 3);

        for px in img.pixels_mut() {
            *px = image::Rgba([1, 2, 3, 255]);
        }

        let mut out = Vec::new();

        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();

        out
    }

    fn find<'a>(groups: &'a [Group], title: &str, label: &str) -> Option<&'a str> {
        groups
            .iter()
            .find(|g| g.title == title)?
            .fields
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, v)| v.as_str())
    }


    #[test]
    fn reads_png_header() {
        let bytes = png_fixture();

        let groups = describe(Path::new("x.png"), &bytes, bytes.len() as u64);

        assert_eq!(find(&groups, "PNG", "Dimensions"), Some("7 x 3 px"));

        assert_eq!(find(&groups, "PNG", "Colour type"), Some("truecolour + alpha"));

        assert!(find(&groups, "File", "Size").is_some());
    }

    /// The whole view exists to inspect files you do not trust, so no input may
    /// panic: truncations at every length, and random bytes behind every magic.
    #[test]
    fn never_panics_on_damaged_input() {
        let png = png_fixture();

        for cut in 0..png.len() {
            let _ = describe(Path::new("x.png"), &png[..cut], cut as u64);
        }

        let magics: &[&[u8]] = &[
            b"\x89PNG\r\n\x1a\n",
            &[0xff, 0xd8, 0xff, 0xe1],
            b"GIF89a",
            b"BM",
            b"RIFF0000WEBPVP8X",
            b"0000ftypmp42",
            b"%PDF-1.7",
            b"PK\x03\x04",
            &[0x1f, 0x8b, 0x08, 0x08],
            b"\x7fELF",
            &[0xcf, 0xfa, 0xed, 0xfe],
            b"\0asm",
            b"ID3\x04",
            b"fLaC",
            b"SQLite format 3\0",
            b"OTTO",
        ];

        // A cheap deterministic pattern generator: no rand dependency, but it
        // still walks every parser over bytes it was not expecting.
        for magic in magics {
            for seed in 0u32..64 {
                let mut bytes = magic.to_vec();

                let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);

                for _ in 0..512 {
                    x ^= x << 13;

                    x ^= x >> 17;

                    x ^= x << 5;

                    bytes.push((x & 0xff) as u8);
                }

                for cut in [bytes.len(), bytes.len() / 2, bytes.len() / 7, magic.len() + 1] {
                    let slice = &bytes[..cut.min(bytes.len())];

                    let _ = describe(Path::new("x.bin"), slice, slice.len() as u64);
                }
            }
        }
    }

    #[test]
    fn exif_survives_a_bogus_tiff_block() {
        for bad in [&b""[..], b"MM", b"MM\0\x2a", b"II\x2a\0\xff\xff\xff\xff", b"XX\0\0"] {
            let _ = exif(bad);
        }
    }

    /// Regressions found by running the readers over real files.
    #[test]
    fn decodes_the_awkward_real_world_cases() {
        // A QuickTime brand is "qt  ": the padding is spaces, not junk.
        assert_eq!(four_cc(b"qt  "), "qt");

        assert_eq!(four_cc(b"mp42"), "mp42");

        assert_eq!(four_cc(&[0x00, 0x01, b'a', b'b']), "..ab");

        // PDF strings are often UTF-16BE behind a BOM; read as bytes they come
        // out as a row of replacement characters.
        let utf16: Vec<u8> = [0xfe, 0xff]
            .into_iter()
            .chain("Hi".encode_utf16().flat_map(|u| u.to_be_bytes()))
            .collect();

        assert_eq!(pdf_text(&utf16).as_deref(), Some("Hi"));

        assert_eq!(pdf_text(b"plain ascii").as_deref(), Some("plain ascii"));

        // Undecodable bytes are dropped rather than shown as mojibake.
        assert_eq!(pdf_text(&[0xff, 0xfe, 0xff, 0xfe, 0xff, 0xfe]), None);

        assert_eq!(exif_date("2026:02:11 10:50:21".to_string()), "2026-02-11 10:50:21");

        // A date that is not in EXIF's shape is left alone.
        assert_eq!(exif_date("unknown".to_string()), "unknown");
    }

    #[test]
    fn formats_durations_and_dates() {
        assert_eq!(duration_text(3661.0), "1:01:01");

        assert_eq!(duration_text(75.0), "1:15");

        assert_eq!(shutter(0.005), "1/200 s");

        assert_eq!(shutter(2.0), "2.0 s");

        // 1970-01-01 plus a day and an hour.
        assert_eq!(civil_from_unix(90_000), "1970-01-02 01:00:00");
    }
}
