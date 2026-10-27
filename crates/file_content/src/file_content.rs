//! Classifies file contents as text or binary and decodes text of any
//! supported encoding into UTF-8, both whole and as a stream.

use std::io::{self, Read};

use anyhow::Result;
use chardetng::EncodingDetector;
use encoding_rs::{CoderResult, Decoder, Encoding, UTF_8, UTF_16BE, UTF_16LE};
use futures_lite::future::yield_now;

/// Number of leading bytes that [`analyze_byte_content`] inspects.
pub const FILE_ANALYSIS_BYTES: usize = 1024;

const DECODING_BLOCK_BYTES: usize = 64 * 1024;

/// Text produced by [`decode_text`], with what is needed to write it back via [`encode_text`].
pub struct DecodedText {
    /// The decoded UTF-8 text.
    pub text: String,
    /// The encoding the bytes were decoded from.
    pub encoding: &'static Encoding,
    /// Whether the bytes started with a byte order mark.
    pub has_bom: bool,
}

pub fn read_byte_header(reader: &mut dyn Read) -> io::Result<(Vec<u8>, bool)> {
    let mut header = vec![0; FILE_ANALYSIS_BYTES];
    let mut length = 0;
    while length < header.len() {
        let read = match reader.read(&mut header[length..]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            header.truncate(length);
            return Ok((header, true));
        }
        length += read;
    }
    Ok((header, false))
}

/// Inspects the start of a file.
/// Returns the BOM encoding, if any, and otherwise the [`ByteContent`] classification of the prefix.
pub fn decode_byte_header(prefix: &[u8]) -> (Option<&'static Encoding>, ByteContent) {
    if let Some((encoding, _bom_len)) = Encoding::for_bom(prefix) {
        return (Some(encoding), ByteContent::Unknown);
    }
    (None, analyze_byte_content(prefix))
}

/// Decodes a whole file into UTF-8, detecting its encoding.
/// Fails when the bytes are classified as [`ByteContent::Binary`].
pub fn decode_text(bytes: Vec<u8>) -> Result<DecodedText> {
    let (bom_encoding, byte_content) = decode_byte_header(&bytes);
    if let Some(encoding) = bom_encoding {
        let (text, _) = encoding.decode_with_bom_removal(&bytes);
        return Ok(DecodedText {
            text: text.into_owned(),
            encoding,
            has_bom: true,
        });
    }

    if let Some(encoding) = byte_content.encoding() {
        let (text, _, _) = encoding.decode(&bytes);
        return Ok(DecodedText {
            text: text.into_owned(),
            encoding,
            has_bom: false,
        });
    }
    anyhow::ensure!(
        byte_content != ByteContent::Binary,
        "Binary files cannot be decoded into text"
    );

    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\x1b') => Ok(DecodedText {
            text,
            encoding: UTF_8,
            has_bom: false,
        }),
        Ok(text) => Ok(decode_with_detected_encoding(text.into_bytes())),
        Err(error) => Ok(decode_with_detected_encoding(error.into_bytes())),
    }
}

pub async fn detect_encoding(reader: &mut (dyn Read + Send)) -> io::Result<&'static Encoding> {
    let mut detector = EncodingDetector::new();
    let mut block = vec![0; DECODING_BLOCK_BYTES];
    let mut length = 0;
    loop {
        let read = match reader.read(&mut block[length..]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            detector.feed(&block[..length], true);
            return Ok(detector.guess(None, true));
        }
        length += read;
        if length == block.len() {
            detector.feed(&block, false);
            yield_now().await;
            length = 0;
        }
    }
}

/// Inverse of [`decode_text`]: encodes UTF-8 text into `encoding`, prepending a BOM when asked.
pub fn encode_text(text: String, encoding: &'static Encoding, has_bom: bool) -> Vec<u8> {
    if encoding == UTF_8 && !has_bom {
        return text.into_bytes();
    }

    // encoding_rs follows the WHATWG standard and encodes UTF-16 labels as UTF-8.
    if encoding == UTF_16BE {
        let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
        if has_bom {
            bytes.extend_from_slice(&[0xFE, 0xFF]);
        }
        bytes.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        return bytes;
    }

    if encoding == UTF_16LE {
        let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
        if has_bom {
            bytes.extend_from_slice(&[0xFF, 0xFE]);
        }
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        return bytes;
    }

    let (encoded, _, _) = encoding.encode(&text);
    if has_bom && encoding == UTF_8 {
        let mut bytes = Vec::with_capacity(encoded.len() + 3);
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        bytes.extend_from_slice(&encoded);
        bytes
    } else {
        encoded.into_owned()
    }
}

/// A [`Read`] adapter that transcodes its source into UTF-8 block by block, with bounded memory.
pub struct DecodingReader<'a> {
    inner: &'a mut (dyn Read + Send),
    decoder: Decoder,
    input: Vec<u8>,
    output: Vec<u8>,
    output_position: usize,
    finished: bool,
}

impl<'a> DecodingReader<'a> {
    pub fn new(inner: &'a mut (dyn Read + Send), encoding: &'static Encoding) -> Self {
        Self {
            inner,
            decoder: encoding.new_decoder(),
            input: vec![0; DECODING_BLOCK_BYTES],
            output: Vec::new(),
            output_position: 0,
            finished: false,
        }
    }

    pub fn encoding(&self) -> &'static Encoding {
        self.decoder.encoding()
    }

    fn fill_output(&mut self) -> io::Result<()> {
        self.output.clear();
        self.output_position = 0;
        let read = self.inner.read(&mut self.input)?;
        self.finished = read == 0;
        decode_block(
            &mut self.decoder,
            &self.input[..read],
            self.finished,
            &mut self.output,
        )
    }
}

impl Read for DecodingReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.output_position == self.output.len() {
            if self.finished {
                return Ok(0);
            }
            match self.fill_output() {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            }
        }

        let available = &self.output[self.output_position..];
        let count = available.len().min(buffer.len());
        buffer[..count].copy_from_slice(&available[..count]);
        self.output_position += count;
        Ok(count)
    }
}

/// Verdict of [`analyze_byte_content`] on a file prefix.
#[derive(Debug, PartialEq)]
pub enum ByteContent {
    /// UTF-16 little-endian text without a BOM.
    Utf16Le,
    /// UTF-16 big-endian text without a BOM.
    Utf16Be,
    /// Not text; the file must not be decoded or searched.
    Binary,
    /// Text in UTF-8 or some yet unknown encoding.
    Unknown,
}

impl ByteContent {
    /// The encoding this classification pins down, if any.
    pub fn encoding(&self) -> Option<&'static Encoding> {
        match self {
            Self::Utf16Le => Some(UTF_16LE),
            Self::Utf16Be => Some(UTF_16BE),
            Self::Binary | Self::Unknown => None,
        }
    }
}

/// Classifies the first [`FILE_ANALYSIS_BYTES`] of a file.
///
/// Heuristic check using null byte distribution plus a generic text-likeness
/// heuristic. This prefers UTF-16 when many bytes are NUL and otherwise
/// distinguishes between text-like and binary-like content.
pub fn analyze_byte_content(bytes: &[u8]) -> ByteContent {
    if bytes.len() < 2 {
        return ByteContent::Unknown;
    }

    if has_binary_header(bytes, KNOWN_BINARY_HEADERS) {
        return ByteContent::Binary;
    }

    let limit = bytes.len().min(FILE_ANALYSIS_BYTES);
    let mut even_null_count = 0usize;
    let mut odd_null_count = 0usize;
    let mut non_text_like_count = 0usize;

    for (i, &byte) in bytes[..limit].iter().enumerate() {
        if byte == 0 {
            if i % 2 == 0 {
                even_null_count += 1;
            } else {
                odd_null_count += 1;
            }
            non_text_like_count += 1;
            continue;
        }

        let is_text_like = match byte {
            b'\t' | b'\n' | b'\r' | 0x0C => true,
            0x20..=0x7E | 0x80..=0xBF | 0xC2..=0xF4 => true,
            _ => false,
        };

        if !is_text_like {
            non_text_like_count += 1;
        }
    }

    let total_null_count = even_null_count + odd_null_count;
    let has_significant_nulls = total_null_count > 0 && total_null_count >= limit / 16;
    let nulls_skew_to_even = even_null_count > odd_null_count * 4;
    let nulls_skew_to_odd = odd_null_count > even_null_count * 4;

    if has_significant_nulls {
        let sample = &bytes[..limit];

        // UTF-16BE ASCII: [0x00, char] — nulls at even positions (high byte first)
        // UTF-16LE ASCII: [char, 0x00] — nulls at odd positions (low byte first)

        if nulls_skew_to_even && is_plausible_utf16_text(sample, false) {
            return ByteContent::Utf16Be;
        }

        if nulls_skew_to_odd && is_plausible_utf16_text(sample, true) {
            return ByteContent::Utf16Le;
        }

        return ByteContent::Binary;
    }

    if has_binary_header(bytes, ADDITIONAL_BINARY_HEADERS)
        || is_bitmap_header(bytes)
        || is_gguf_header(bytes)
        || is_safetensors_header(bytes)
    {
        return ByteContent::Binary;
    }

    if total_null_count == 0 || non_text_like_count * 100 < limit * 8 {
        ByteContent::Unknown
    } else {
        ByteContent::Binary
    }
}

fn decode_block(
    decoder: &mut Decoder,
    block: &[u8],
    last: bool,
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let capacity = decoder
        .max_utf8_buffer_length(block.len())
        .ok_or_else(|| io::Error::new(io::ErrorKind::OutOfMemory, "decoded block is too large"))?;
    let start = output.len();
    output.resize(start + capacity, 0);
    let (result, consumed, written, _) = decoder.decode_to_utf8(block, &mut output[start..], last);
    output.truncate(start + written);
    if result != CoderResult::InputEmpty || consumed != block.len() {
        return Err(io::Error::other("decoder did not consume the whole block"));
    }
    Ok(())
}

fn decode_with_detected_encoding(bytes: Vec<u8>) -> DecodedText {
    let mut detector = EncodingDetector::new();
    detector.feed(&bytes, true);
    let encoding = detector.guess(None, true);
    let (text, _, _) = encoding.decode(&bytes);
    DecodedText {
        text: text.into_owned(),
        encoding,
        has_bom: false,
    }
}

const KNOWN_BINARY_HEADERS: &[&[u8]] = &[
    b"%PDF-",             // PDF
    b"GIF87a",            // GIF87a
    b"GIF89a",            // GIF89a
    b"ID3",               // MP3 with ID3v2 tag
    b"IWAD",              // Doom IWAD archive
    b"OggS",              // OGG (Vorbis, Opus, FLAC)
    b"PK\x03\x04",        // ZIP local header
    b"PK\x05\x06",        // ZIP end of central directory
    b"PK\x07\x08",        // ZIP spanning/splitting
    b"PWAD",              // Doom PWAD archive
    b"RIFF",              // WAV, AVI, WebP
    b"fLaC",              // FLAC
    b"\x89PNG\r\n\x1a\n", // PNG
    b"\xFF\xD8\xFF",      // JPEG
    b"\xFF\xF2",          // MP3 frame sync (MPEG2 Layer3)
    b"\xFF\xF3",          // MP3 frame sync (MPEG2 Layer3)
    b"\xFF\xFA",          // MP3 frame sync (MPEG1 Layer3)
    b"\xFF\xFB",          // MP3 frame sync (MPEG1 Layer3)
];

const ADDITIONAL_BINARY_HEADERS: &[&[u8]] = &[
    b"\x00\x00\x00\x0CJXL \r\n\x87\n", // JPEG XL container
    b"\x00\x00\x00\x0CjP  ",           // JPEG 2000
    b"\x00\x00\x01\x00",               // Windows ICO
    b"\x00\x00\x02\x00",               // Windows CUR
    b"\x00\x01\x00\x00\x00",           // TrueType font
    b"\x00asm",                        // WebAssembly
    b"\x04\x22\x4D\x18",               // LZ4 frame
    b"\x1A\x45\xDF\xA3",               // Matroska, WebM
    b"\x1F\x8B\x08",                   // gzip
    b"7z\xBC\xAF\x27\x1C",             // 7-Zip
    b"8BPS\x00",                       // Photoshop PSD
    b"DDS |\x00\x00\x00",              // DirectDraw Surface
    b"II*\x00",                        // TIFF little-endian
    b"Kaydara FBX Binary  \x00",       // FBX binary
    b"MM\x00*",                        // TIFF big-endian
    b"MSCF\x00\x00\x00\x00",           // Microsoft Cabinet
    b"OTTO\x00",                       // OpenType font with CFF outlines
    b"Rar!\x1A\x07",                   // RAR
    b"SQLite format 3\x00",            // SQLite database
    b"glTF\x01\x00\x00\x00",           // Binary glTF 1
    b"glTF\x02\x00\x00\x00",           // Binary glTF 2
    b"\x7FELF",                        // ELF
    b"\x89HDF\r\n\x1a\n",              // HDF5
    b"\x93NUMPY\x01\x00",              // NumPy array
    b"\x93NUMPY\x02\x00",
    b"\x93NUMPY\x03\x00",
    b"\x95\x04\x12\xDE",                 // gettext MO big-endian
    b"\xA1\xB2\xC3\xD4\x00\x02\x00\x04", // pcap big-endian
    b"\xABKTX 11\xBB\r\n\x1A\n",         // KTX texture
    b"\xABKTX 20\xBB\r\n\x1A\n",
    b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1", // OLE2 compound file (legacy MS Office)
    b"\xD4\xC3\xB2\xA1\x02\x00\x04\x00", // pcap little-endian
    b"\xDE\x12\x04\x95",                 // gettext MO little-endian
    b"\xFD7zXZ\x00",                     // xz
];

fn has_binary_header(bytes: &[u8], headers: &[&[u8]]) -> bool {
    let index = headers.partition_point(|header| *header <= bytes);
    headers[..index]
        .last()
        .is_some_and(|header| bytes.starts_with(header))
}

fn is_gguf_header(bytes: &[u8]) -> bool {
    bytes.starts_with(b"GGUF") && bytes.get(5..8) == Some(&[0, 0, 0])
}

fn is_bitmap_header(bytes: &[u8]) -> bool {
    bytes.starts_with(b"BM") && bytes.get(6..10) == Some(&[0, 0, 0, 0])
}

fn is_safetensors_header(bytes: &[u8]) -> bool {
    bytes.get(3..8) == Some(&[0, 0, 0, 0, 0]) && bytes.get(8) == Some(&b'{')
}

// Null byte skew alone is not enough to identify UTF-16 -- binary formats with
// small 16-bit values (like PCM audio) produce the same pattern. Decode the
// bytes as UTF-16 and reject if too many code units land in control character
// ranges or form unpaired surrogates, which real text almost never contains.
fn is_plausible_utf16_text(bytes: &[u8], little_endian: bool) -> bool {
    let mut suspicious_count = 0usize;
    let mut word_like_count = 0usize;
    let mut total = 0usize;

    let mut i = 0;
    while let Some(code_unit) = read_u16(bytes, i, little_endian) {
        total += 1;

        match code_unit {
            0x0009 | 0x000A | 0x000C | 0x000D => {}
            0x0020 | 0x0030..=0x0039 | 0x0041..=0x005A | 0x0061..=0x007A => {
                word_like_count += 1;
            }
            // C0/C1 control characters and non-characters
            0x0000..=0x001F | 0x007F..=0x009F | 0xFFFE | 0xFFFF => suspicious_count += 1,
            0xD800..=0xDBFF => {
                let next_offset = i + 2;
                let has_low_surrogate = read_u16(bytes, next_offset, little_endian)
                    .is_some_and(|next| (0xDC00..=0xDFFF).contains(&next));
                if has_low_surrogate {
                    total += 1;
                    word_like_count += 2;
                    i += 2;
                } else {
                    suspicious_count += 1;
                }
            }
            // Lone low surrogate without a preceding high surrogate
            0xDC00..=0xDFFF => suspicious_count += 1,
            0x0100.. => word_like_count += 1,
            _ => {}
        }

        i += 2;
    }

    if total == 0 {
        return false;
    }

    // Real UTF-16 text has near-zero control characters; binary data with
    // small 16-bit values typically exceeds 5%. 2% provides a safe margin.
    let low_control_ratio = suspicious_count * 100 < total * 2;

    // Binary formats that interleave short ASCII fragments with small
    // length/type fields (e.g. game asset formats) can dodge the control
    // character check above while barely containing any real words: their
    // code units land on ASCII punctuation and Latin-1 symbol values rather
    // than letters, digits, or spaces. Real text is overwhelmingly made of
    // word characters, so require a minimum share of them. Code units above
    // the Latin-1 range (and surrogate pairs) also count as word-like so
    // that scripts such as Cyrillic or Greek, whose letters are non-ASCII,
    // are still recognized -- tag bytes paired with a zero byte can never
    // land there.
    let enough_word_chars = word_like_count * 100 >= total * 30;

    low_control_ratio && enough_word_chars
}

fn read_u16(bytes: &[u8], offset: usize, little_endian: bool) -> Option<u16> {
    let pair = [*bytes.get(offset)?, *bytes.get(offset + 1)?];
    if little_endian {
        return Some(u16::from_le_bytes(pair));
    }
    Some(u16::from_be_bytes(pair))
}

