//! Non-text files: classify a path as text, image or other binary, and (for
//! images) re-encode a terminal-sized PNG ready for the kitty graphics
//! protocol that Ghostty speaks.

use std::fs;
use std::io::{Read, Cursor};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use image::ImageFormat;

/// Longest side (px) an image is re-sampled down to before transmission —
/// keeps the payload small with no visible loss at terminal sizes.
const MAX_DIM: u32 = 1000;

/// Bytes sniffed to classify a file (and, for binaries, to hex-preview).
const SNIFF: usize = 8192;

/// Bytes read for metadata. Larger than the classify sniff because a JPEG's
/// EXIF block alone can run to tens of kilobytes.
const META_PREFIX: usize = 64 * 1024;

/// Bytes per hex row, and how much is pulled off disk around the visible rows
/// so scrolling does not hit the file on every frame.
pub const HEX_COLS: usize = 16;
const HEX_WINDOW: usize = 64 * 1024;

/// base64 chunk size for the kitty protocol — must be ≤4096 and a multiple of
/// 4 so each chunk stays on a base64 boundary.
const KITTY_CHUNK: usize = 4096;

pub enum Media {
    Image(ImageDoc),

    Binary(BinaryDoc),
}

pub struct ImageDoc {
    pub path: PathBuf,

    pub format: String,

    pub byte_len: u64,

    pub width: u32,

    pub height: u32,

    pub meta: Vec<crate::meta::Group>,

    /// Re-encoded PNG, base64'd straight into the kitty escape.
    png: Vec<u8>,
}

pub struct BinaryDoc {
    pub path: PathBuf,

    pub format: String,

    pub byte_len: u64,

    pub meta: Vec<crate::meta::Group>,

    /// Bytes currently held for the hex view, and the offset they start at.
    /// The file is never loaded whole: only a window around what is on screen,
    /// so a multi-gigabyte file costs the same as a small one.
    window: Vec<u8>,

    window_at: u64,
}

impl BinaryDoc {
    /// Number of 16-byte rows the whole file occupies.
    pub fn hex_rows(&self) -> u64 {
        self.byte_len.div_ceil(HEX_COLS as u64)
    }

    /// Make sure `rows` rows starting at `first_row` are in memory. Reads only
    /// when the request falls outside what is already held.
    pub fn ensure_window(&mut self, first_row: u64, rows: usize) {
        let want_at = first_row.saturating_mul(HEX_COLS as u64);

        let want_len = rows.saturating_mul(HEX_COLS);

        let have_end = self.window_at + self.window.len() as u64;

        if want_at >= self.window_at && want_at + want_len as u64 <= have_end {
            return;
        }

        // Centre the window on the request so scrolling either way stays cheap.
        let back = (HEX_WINDOW.saturating_sub(want_len) / 2) as u64;

        let at = want_at.saturating_sub(back);

        if let Ok(bytes) = read_window(&self.path, at, HEX_WINDOW.max(want_len)) {
            self.window = bytes;

            self.window_at = at;
        }
    }

    /// The bytes of one hex row, or an empty slice when they are not loaded.
    pub fn hex_row(&self, row: u64) -> &[u8] {
        let at = row.saturating_mul(HEX_COLS as u64);

        let Some(rel) = at.checked_sub(self.window_at) else {
            return &[];
        };

        let rel = rel as usize;

        let end = (rel + HEX_COLS).min(self.window.len());

        self.window.get(rel..end).unwrap_or(&[])
    }
}

/// One rendered row of the inspector, tagged so the renderer can colour it
/// without re-deciding what it is looking at.
pub enum Row {
    /// Group heading, e.g. "Camera".
    Title(String),

    /// A `label: value` pair.
    Field(String, String),

    Blank,
}

/// Lay metadata groups out as rows. The hex rows are appended by the caller,
/// which knows how far the file runs.
pub fn meta_rows(groups: &[crate::meta::Group]) -> Vec<Row> {
    let mut rows = vec![Row::Blank];

    for group in groups {
        if group.fields.is_empty() {
            continue;
        }

        if rows.len() > 1 {
            rows.push(Row::Blank);
        }

        rows.push(Row::Title(group.title.clone()));

        for (label, value) in &group.fields {
            rows.push(Row::Field(label.clone(), value.clone()));
        }
    }

    rows
}

/// Printable rendering of a hex row's bytes: `hex bytes  |ascii|`.
pub fn hex_line(offset: u64, bytes: &[u8]) -> (String, String) {
    let mut hex = String::with_capacity(HEX_COLS * 3 + 1);

    for i in 0..HEX_COLS {
        // A blank gutter down the middle keeps the eye on 8-byte boundaries.
        if i == HEX_COLS / 2 {
            hex.push(' ');
        }

        match bytes.get(i) {
            Some(b) => hex.push_str(&format!("{b:02x} ")),

            None => hex.push_str("   "),
        }
    }

    let ascii: String = bytes
        .iter()
        .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
        .collect();

    (format!("    {offset:08x}  {hex}"), format!("|{ascii}|"))
}

fn read_window(path: &Path, at: u64, len: usize) -> Result<Vec<u8>> {
    use std::io::{Seek, SeekFrom};

    let mut file = fs::File::open(path)?;

    file.seek(SeekFrom::Start(at))?;

    let mut buf = vec![0u8; len];

    let mut filled = 0;

    while filled < len {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,

            Ok(n) => filled += n,

            Err(e) => return Err(e.into()),
        }
    }

    buf.truncate(filled);

    Ok(buf)
}

pub enum Loaded {
    Text,

    Media(Media),
}

/// Decide how to open `path`. Reads only a prefix unless the file turns out to
/// be an image (which must be decoded whole).
pub fn classify(path: &Path) -> Result<Loaded> {
    let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;

    let byte_len = meta.len();

    let mut prefix = read_prefix(path, SNIFF)?;

    if let Ok(format) = image::guess_format(&prefix) {
        if let Some(doc) = decode_image(path, byte_len, format) {
            return Ok(Loaded::Media(Media::Image(doc)));
        }
    }

    // A known binary magic wins over the UTF-8 sniff: some binaries (e.g. a PDF
    // header) start with plain ASCII yet are not text.
    if magic_format(&prefix).is_none() && looks_textual(&prefix) {
        return Ok(Loaded::Text);
    }

    let format = describe_binary(path, &prefix);

    // Re-read a longer prefix for the metadata readers; the sniff window is
    // sized for classification, not for a JPEG's EXIF block.
    if prefix.len() == SNIFF && byte_len > SNIFF as u64 {
        if let Ok(longer) = read_prefix(path, META_PREFIX) {
            prefix = longer;
        }
    }

    let meta = crate::meta::describe(path, &prefix, byte_len);

    Ok(Loaded::Media(Media::Binary(BinaryDoc {
        path: path.to_path_buf(),
        format,
        byte_len,
        meta,
        window: Vec::new(),
        window_at: 0,
    })))
}

fn read_prefix(path: &Path, max: usize) -> Result<Vec<u8>> {
    let mut file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;

    let mut buf = vec![0u8; max];

    let mut filled = 0;

    while filled < max {
        let n = file.read(&mut buf[filled..])?;

        if n == 0 {
            break;
        }

        filled += n;
    }

    buf.truncate(filled);

    Ok(buf)
}

/// Valid UTF-8, tolerating a multibyte char clipped by the sniff window.
fn looks_textual(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,

        Err(e) => e.error_len().is_none() && e.valid_up_to() > 0,
    }
}

fn decode_image(path: &Path, byte_len: u64, format: ImageFormat) -> Option<ImageDoc> {
    let bytes = fs::read(path).ok()?;

    let img = image::load_from_memory_with_format(&bytes, format).ok()?;

    // Report the real dimensions, not the ones left after down-sampling for
    // transmission: those are an artefact of the terminal, not of the file.
    let (width, height) = (img.width(), img.height());

    let scaled = if width.max(height) > MAX_DIM {
        img.resize(MAX_DIM, MAX_DIM, image::imageops::FilterType::Triangle)
    } else {
        img
    };

    let mut png = Vec::new();

    scaled.write_to(&mut Cursor::new(&mut png), ImageFormat::Png).ok()?;

    Some(ImageDoc {
        path: path.to_path_buf(),
        format: format_name(format),
        byte_len,
        width,
        height,
        meta: crate::meta::describe(path, &bytes, byte_len),
        png,
    })
}

fn format_name(format: ImageFormat) -> String {
    match format {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WebP",
        ImageFormat::Bmp => "BMP",
        _ => "image",
    }
    .to_string()
}

/// Identify a file by its leading magic bytes, when we recognise it.
fn magic_format(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"%PDF") {
        Some("PDF document")
    } else if head.starts_with(b"PK\x03\x04") {
        Some("ZIP archive")
    } else if head.starts_with(&[0x1f, 0x8b]) {
        Some("gzip archive")
    } else if head.starts_with(b"\x7fELF") {
        Some("ELF binary")
    } else if head.starts_with(&[0xca, 0xfe, 0xba, 0xbe]) || head.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
        Some("Mach-O binary")
    } else if head.starts_with(b"\0asm") {
        Some("WebAssembly module")
    } else {
        None
    }
}

fn describe_binary(path: &Path, head: &[u8]) -> String {
    if let Some(name) = magic_format(head) {
        return name.to_string();
    }

    let by_ext = match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp3" | "wav" | "flac" | "ogg" | "m4a") => "audio file",
        Some("mp4" | "mov" | "mkv" | "webm" | "avi") => "video file",
        Some("ttf" | "otf" | "woff" | "woff2") => "font file",
        Some("zip" | "tar" | "7z" | "rar") => "archive",
        _ => "binary file",
    };

    by_ext.to_string()
}

impl ImageDoc {
    /// The cell box the image actually occupies inside a `cols`×`rows` area,
    /// aspect preserved, so the caller can centre it in the leftover space.
    pub fn fitted_cells(&self, cols: u16, rows: u16) -> (u16, u16) {
        fit_cells(self.width, self.height, cols, rows)
    }

    /// kitty escape(s) to transmit + display the image, scaled to fit a
    /// `cols`×`rows` cell box at the current cursor position (aspect preserved,
    /// cursor left in place so it never scrolls the view).
    pub fn kitty_sequence(&self, cols: u16, rows: u16) -> Vec<u8> {
        let (c, r) = fit_cells(self.width, self.height, cols, rows);

        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.png);

        let bytes = b64.as_bytes();

        let total = bytes.len();

        let mut out = Vec::with_capacity(total + 256);

        let mut i = 0;

        while i < total {
            let end = (i + KITTY_CHUNK).min(total);

            let more = u8::from(end < total);

            out.extend_from_slice(b"\x1b_G");

            if i == 0 {
                let header = format!("a=T,f=100,c={c},r={r},C=1,q=2,m={more};");

                out.extend_from_slice(header.as_bytes());
            } else {
                let header = format!("m={more};");

                out.extend_from_slice(header.as_bytes());
            }

            out.extend_from_slice(&bytes[i..end]);

            out.extend_from_slice(b"\x1b\\");

            i = end;
        }

        out
    }
}

/// Fit `w`×`h` pixels into at most `cols`×`rows` cells, assuming a cell is about
/// twice as tall as it is wide so pixels stay roughly square.
fn fit_cells(w: u32, h: u32, cols: u16, rows: u16) -> (u16, u16) {
    if w == 0 || h == 0 || cols == 0 || rows == 0 {
        return (cols.max(1), rows.max(1));
    }

    let cols_per_row = (w as f64 / h as f64) * 2.0;

    let want_cols = (rows as f64 * cols_per_row).round();

    if want_cols <= cols as f64 {
        ((want_cols.max(1.0)) as u16, rows)
    } else {
        let want_rows = (cols as f64 / cols_per_row).round().clamp(1.0, rows as f64);

        (cols, want_rows as u16)
    }
}

/// Delete every kitty image placement (on leaving an image view or quitting).
pub fn kitty_delete() -> &'static [u8] {
    b"\x1b_Ga=d,q=2\x1b\\"
}

/// Human-readable byte count for the status bar.
pub fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut size = n as f64;

    let mut unit = 0;

    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;

        unit += 1;
    }

    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_text_and_binary() {
        let dir = std::env::temp_dir().join(format!("ocode_media_{}", std::process::id()));

        fs::create_dir_all(&dir).unwrap();

        let txt = dir.join("a.rs");

        fs::write(&txt, "fn main() {}\n").unwrap();

        assert!(matches!(classify(&txt).unwrap(), Loaded::Text));

        let pdf = dir.join("b.pdf");

        fs::write(&pdf, b"%PDF-1.4\n\x00\x01\x02binary").unwrap();

        match classify(&pdf).unwrap() {
            Loaded::Media(Media::Binary(d)) => assert_eq!(d.format, "PDF document"),

            _ => panic!("expected binary"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn classifies_png_as_image() {
        let dir = std::env::temp_dir().join(format!("ocode_media_png_{}", std::process::id()));

        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("p.png");

        let mut img = image::RgbaImage::new(20, 10);

        for px in img.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }

        image::DynamicImage::ImageRgba8(img).save(&path).unwrap();

        match classify(&path).unwrap() {
            Loaded::Media(Media::Image(doc)) => {
                assert_eq!(doc.format, "PNG");

                assert_eq!((doc.width, doc.height), (20, 10));

                let seq = doc.kitty_sequence(80, 24);

                assert!(seq.starts_with(b"\x1b_Ga=T,f=100,"), "kitty header missing");

                assert!(seq.windows(2).any(|w| w == b"\x1b\\"), "kitty terminator missing");
            }

            _ => panic!("expected image"),
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fit_cells_is_idempotent() {
        // The placement fits once to centre the image, and kitty_sequence fits
        // again on the way out; the second pass must not shrink it further.
        for (w, h) in [(1000u32, 100u32), (100, 1000), (640, 480), (1, 1), (20, 10)] {
            let (c, r) = fit_cells(w, h, 80, 24);

            assert_eq!(fit_cells(w, h, c, r), (c, r), "for {w}x{h}");
        }
    }

    #[test]
    fn fit_cells_preserves_orientation() {
        // wide image -> limited by columns
        let (c, r) = fit_cells(1000, 100, 80, 24);

        assert!(c <= 80 && r <= 24 && c >= r);

        // tall image -> limited by rows
        let (c, r) = fit_cells(100, 1000, 80, 24);

        assert!(c <= 80 && r <= 24 && r >= c);
    }

    /// The hex view must reach the end of a file far larger than any window,
    /// reading the right bytes at whatever offset is asked for.
    #[test]
    fn hex_window_reads_any_offset_without_loading_the_file() {
        let dir = std::env::temp_dir().join(format!("ocode_hex_{}", std::process::id()));

        let _ = fs::create_dir_all(&dir);

        let path = dir.join("big.bin");

        // Larger than the read window, with a pattern that identifies any byte
        // by its own offset.
        let len = 300_000usize;

        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

        fs::write(&path, &data).unwrap();

        let Loaded::Media(Media::Binary(mut doc)) = classify(&path).unwrap() else {
            panic!("a file of bytes should not classify as text");
        };

        assert_eq!(doc.byte_len, len as u64);

        assert_eq!(doc.hex_rows(), (len as u64).div_ceil(HEX_COLS as u64));

        // Walk the start, the far end and back again: the window has to move
        // both ways, and the bytes must still match the pattern.
        for row in [0u64, 5, 10_000, doc.hex_rows() - 1, 3, 9_999] {
            doc.ensure_window(row, 24);

            let bytes = doc.hex_row(row);

            let at = row as usize * HEX_COLS;

            let want = &data[at..(at + HEX_COLS).min(len)];

            assert_eq!(bytes, want, "row {row} at offset {at:#x}");
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hex_line_pads_a_short_final_row() {
        let (hex, ascii) = hex_line(0x10, &[0x41, 0x00, 0x7e]);

        // Indented to line up with the metadata fields above the dump.
        assert!(hex.starts_with("    00000010  41 00 7e"), "offset and bytes: {hex}");

        // The short row still lines up with the full ones above it.
        let (full, _) = hex_line(0, &[0u8; HEX_COLS]);

        assert_eq!(hex.len(), full.len(), "short rows keep the column width");

        assert_eq!(ascii, "|A.~|", "printable bytes only, the rest as dots");
    }

    #[test]
    fn human_size_reads_well() {
        assert_eq!(human_size(512), "512 B");

        assert_eq!(human_size(2048), "2.0 KB");
    }
}
