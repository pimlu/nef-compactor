/// CNEF container format — lossless compressed NEF.
///
/// The file is split into ordered segments at chunk boundaries. Each segment
/// is compressed independently. Decompression concatenates the segments in
/// order, reconstructing the original NEF byte-for-byte.
///
/// ```text
/// Header:
///   magic: [u8; 4] = "CNEF"
///   version: u8 = 1
///   original_file_size: u64 LE
///   segment_count: u32 LE
///
/// Per segment (header then payload, in sequence):
///   type: u8
///   original_offset: u64 LE
///   original_length: u64 LE
///   compressed_length: u64 LE
///   -- if type == RAW_PIXELS (2): raw strip metadata follows --
///     width: u32 LE
///     height: u32 LE
///     bits_per_sample: u8
///     huff_select: u8
///     split_row: u32 LE
///     initial_predictors: [i32; 4] LE  (pUp[0][0], pUp[0][1], pUp[1][0], pUp[1][1])
///   [payload: compressed_length bytes]
/// ```

use std::io::{Read, Seek, SeekFrom, Write};

use crate::jxl;
use crate::nef::{NefChunks, NikonLosslessMeta};
use crate::nikon_lossless;
use crate::packed;

const MAGIC: &[u8; 4] = b"CNEF";
const VERSION: u8 = 1;

const ZSTD_LEVEL: i32 = 19;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    Zstd = 1,
    RawPixelsJxl = 2,
    JpegJxl = 3,
    /// Bit-packed uncompressed raw, recompressed as lossless JPEG XL. Added
    /// after v1; old readers reject it via `from_u8` (the file simply requires a
    /// newer binary), so no format version bump is needed.
    RawUncompressedJxl = 4,
    /// Like `RawPixelsJxl`, but the stored values are biased into a compact
    /// non-negative range (lossy frames can run the predictor out of range);
    /// the payload is prefixed with the `i32` bias. Also added after v1.
    RawPixelsBiasedJxl = 5,
}

impl SegmentType {
    fn from_u8(v: u8) -> Result<Self, String> {
        match v {
            1 => Ok(Self::Zstd),
            2 => Ok(Self::RawPixelsJxl),
            3 => Ok(Self::JpegJxl),
            4 => Ok(Self::RawUncompressedJxl),
            5 => Ok(Self::RawPixelsBiasedJxl),
            _ => Err(format!("unknown segment type {v}")),
        }
    }

    fn has_raw_meta(self) -> bool {
        matches!(
            self,
            Self::RawPixelsJxl | Self::RawUncompressedJxl | Self::RawPixelsBiasedJxl
        )
    }
}

/// Choose how to feed decoded lossless pixels to JXL. Normally the stored
/// values are in `[0, 2^bps)` and go in unchanged (`bias` is `None`). A lossy
/// frame can run the predictor out of range — values wrap near 65535 — which
/// wrecks JXL; shift them by the minimum into a compact non-negative range and
/// return the bias so decompression can undo it. The shift cancels out of the
/// prediction diffs, so the re-encode is unaffected.
fn bias_pixels(pixels: Vec<u16>, bps: u32) -> (Vec<u16>, u32, Option<i32>) {
    let limit = 1i32 << bps;
    let signed = |p: u16| p as i16 as i32; // recover the (possibly negative) predictor
    let mut lo = i32::MAX;
    let mut hi = i32::MIN;
    let mut out_of_range = false;
    for &p in &pixels {
        let s = signed(p);
        lo = lo.min(s);
        hi = hi.max(s);
        if s < 0 || s >= limit {
            out_of_range = true;
        }
    }
    if !out_of_range {
        return (pixels, bps, None);
    }
    let biased: Vec<u16> = pixels.iter().map(|&p| (signed(p) - lo) as u16).collect();
    let span = (hi - lo) as u32; // < 2^16 since signed values fit in i16
    let jxl_bps = (32 - span.leading_zeros()).max(1);
    (biased, jxl_bps, Some(lo))
}

/// Split a raw payload `[jxl_len: u32 LE][jxl_data][tail]` into (jxl, tail).
fn split_jxl_payload(payload: &[u8]) -> (&[u8], &[u8]) {
    if payload.len() >= 4 {
        let jxl_len = u32::from_le_bytes(payload[..4].try_into().unwrap()) as usize;
        if jxl_len + 4 <= payload.len() {
            return (&payload[4..4 + jxl_len], &payload[4 + jxl_len..]);
        }
    }
    (payload, &[])
}

/// How the original raw strip is encoded, decided by the caller before
/// compression. Lossless is the Nikon Huffman format; Uncompressed is
/// bit-packed samples (some bodies mislabel these as Nikon-compressed).
pub enum RawSource {
    Lossless(NikonLosslessMeta),
    Uncompressed,
}

struct Segment {
    seg_type: SegmentType,
    original_offset: u64,
    original_length: u64,
    payload: Vec<u8>,
    raw_meta: Option<RawPixelsMeta>,
}

#[derive(Clone)]
struct RawPixelsMeta {
    width: u32,
    height: u32,
    bits_per_sample: u8,
    huff_select: u8,
    split_row: u32,
    initial_predictors: [i32; 4],
}

pub fn compress<R: Read + Seek, W: Write>(
    nef: &mut R,
    chunks: &NefChunks,
    raw_source: &RawSource,
    effort: i64,
    out: &mut W,
) -> Result<CompressionStats, String> {
    let mut regions: Vec<(u64, u64, RegionKind)> = Vec::new();

    // Collect all chunk regions. The raw strip is always Nikon lossless here —
    // callers reject other formats before reaching this point.
    regions.push((
        chunks.raw_strip.offset,
        chunks.raw_strip.length,
        RegionKind::RawStrip,
    ));
    for jpeg in &chunks.jpegs {
        regions.push((jpeg.offset, jpeg.length as u64, RegionKind::Jpeg));
    }

    regions.sort_by_key(|r| r.0);

    // Build segments: interleave skeleton gaps with chunk regions
    let mut segments: Vec<Segment> = Vec::new();
    let mut pos: u64 = 0;

    for &(offset, length, kind) in &regions {
        if offset > pos {
            let gap = read_range(nef, pos, (offset - pos) as usize)?;
            segments.push(Segment {
                seg_type: SegmentType::Zstd,
                original_offset: pos,
                original_length: offset - pos,
                payload: zstd_compress(&gap)?,
                raw_meta: None,
            });
        }

        match kind {
            RegionKind::RawStrip => {
                let compressed_raw = read_range(nef, offset, length as usize)?;
                let width = chunks.raw_strip.width as usize;
                let height = chunks.raw_strip.height as usize;
                let bps = chunks.raw_strip.bits_per_sample;

                let (seg_type, pixels, tail, jxl_bps, prefix) = match raw_source {
                    RawSource::Lossless(meta) => {
                        let decode_result = nikon_lossless::decode(
                            &compressed_raw,
                            width,
                            height,
                            bps,
                            meta.huff_select,
                            meta.initial_predictors,
                            meta.split_row,
                        )?;

                        // Split at the last fully-consumed byte boundary; the
                        // partial final byte plus any trailing data is the
                        // "tail" we preserve verbatim.
                        let full_bytes =
                            (decode_result.bits_consumed / 8).min(compressed_raw.len());
                        let tail = compressed_raw[full_bytes..].to_vec();

                        // Confirm the strip round-trips bit-exactly before the
                        // expensive JXL encode. Any failure — a mismatch or an
                        // encode error (a diff category absent from the table,
                        // i.e. the data isn't really our format) — means an
                        // unsupported variant: skip cleanly here rather than
                        // emit a broken CNEF or fail verify late.
                        let round_trips = reconstruct_raw_strip(
                            &decode_result.pixels,
                            width,
                            height,
                            bps,
                            meta.huff_select,
                            meta.initial_predictors,
                            meta.split_row,
                            &tail,
                            length as usize,
                        )
                        .map(|check| check == compressed_raw)
                        .unwrap_or(false);
                        if !round_trips {
                            return Err("unsupported Nikon lossless variant (raw strip does not round-trip)".into());
                        }
                        // Lossy frames can run the predictor out of range; bias
                        // such values into a compact range (recorded in the
                        // payload) so JXL isn't fed wrap-around spikes.
                        let (jxl_pixels, jxl_bps, bias) = bias_pixels(decode_result.pixels, bps);
                        match bias {
                            Some(b) => (
                                SegmentType::RawPixelsBiasedJxl,
                                jxl_pixels,
                                tail,
                                jxl_bps,
                                b.to_le_bytes().to_vec(),
                            ),
                            None => (SegmentType::RawPixelsJxl, jxl_pixels, tail, jxl_bps, Vec::new()),
                        }
                    }
                    RawSource::Uncompressed => {
                        let layout = packed::Layout::detect(length as usize, width, height, bps)
                            .ok_or("uncompressed raw: irregular row layout")?;
                        let pixels = packed::unpack(&compressed_raw, width, height, layout);
                        // The samples must repack to the exact original bytes
                        // (i.e. padding is zero); otherwise skip cleanly.
                        if packed::pack(&pixels, width, height, layout) != compressed_raw {
                            return Err(
                                "uncompressed raw does not repack exactly (non-zero padding?)".into(),
                            );
                        }
                        (
                            SegmentType::RawUncompressedJxl,
                            pixels,
                            Vec::new(),
                            layout.jxl_bps(),
                            Vec::new(),
                        )
                    }
                };

                let compressed_pixels = jxl::encode_pixels(
                    &pixels,
                    chunks.raw_strip.width,
                    chunks.raw_strip.height,
                    jxl_bps,
                    effort,
                )?;

                let mut raw_payload =
                    Vec::with_capacity(prefix.len() + 4 + compressed_pixels.len() + tail.len());
                raw_payload.extend_from_slice(&prefix);
                raw_payload.extend_from_slice(&(compressed_pixels.len() as u32).to_le_bytes());
                raw_payload.extend_from_slice(&compressed_pixels);
                raw_payload.extend_from_slice(&tail);

                let raw_meta = match raw_source {
                    RawSource::Lossless(meta) => RawPixelsMeta {
                        width: chunks.raw_strip.width,
                        height: chunks.raw_strip.height,
                        bits_per_sample: bps as u8,
                        huff_select: meta.huff_select as u8,
                        split_row: meta.split_row as u32,
                        initial_predictors: [
                            meta.initial_predictors[0][0],
                            meta.initial_predictors[0][1],
                            meta.initial_predictors[1][0],
                            meta.initial_predictors[1][1],
                        ],
                    },
                    RawSource::Uncompressed => RawPixelsMeta {
                        width: chunks.raw_strip.width,
                        height: chunks.raw_strip.height,
                        bits_per_sample: bps as u8,
                        huff_select: 0,
                        split_row: 0,
                        initial_predictors: [0; 4],
                    },
                };

                segments.push(Segment {
                    seg_type,
                    original_offset: offset,
                    original_length: length,
                    payload: raw_payload,
                    raw_meta: Some(raw_meta),
                });
            }
            RegionKind::Jpeg => {
                let jpeg_data = read_range(nef, offset, length as usize)?;
                let jxl_data = jxl::encode_jpeg(&jpeg_data)?;
                segments.push(Segment {
                    seg_type: SegmentType::JpegJxl,
                    original_offset: offset,
                    original_length: length,
                    payload: jxl_data,
                    raw_meta: None,
                });
            }
        }

        pos = offset + length;
    }

    // Trailing skeleton after last chunk
    if pos < chunks.file_size {
        let gap = read_range(nef, pos, (chunks.file_size - pos) as usize)?;
        segments.push(Segment {
            seg_type: SegmentType::Zstd,
            original_offset: pos,
            original_length: chunks.file_size - pos,
            payload: zstd_compress(&gap)?,
            raw_meta: None,
        });
    }

    // Write CNEF
    let mut total_compressed: u64 = 0;
    let mut total_original: u64 = 0;
    let mut seg_stats: Vec<SegmentStats> = Vec::new();

    // Header
    out.write_all(MAGIC).w()?;
    out.write_all(&[VERSION]).w()?;
    out.write_all(&chunks.file_size.to_le_bytes()).w()?;
    out.write_all(&(segments.len() as u32).to_le_bytes()).w()?;

    for seg in &segments {
        out.write_all(&[seg.seg_type as u8]).w()?;
        out.write_all(&seg.original_offset.to_le_bytes()).w()?;
        out.write_all(&seg.original_length.to_le_bytes()).w()?;
        out.write_all(&(seg.payload.len() as u64).to_le_bytes()).w()?;

        if let Some(ref m) = seg.raw_meta {
            out.write_all(&m.width.to_le_bytes()).w()?;
            out.write_all(&m.height.to_le_bytes()).w()?;
            out.write_all(&[m.bits_per_sample]).w()?;
            out.write_all(&[m.huff_select]).w()?;
            out.write_all(&m.split_row.to_le_bytes()).w()?;
            for &p in &m.initial_predictors {
                out.write_all(&p.to_le_bytes()).w()?;
            }
        }

        out.write_all(&seg.payload).w()?;

        let payload_len = seg.payload.len() as u64;
        seg_stats.push(SegmentStats {
            seg_type: seg.seg_type,
            original_size: seg.original_length,
            compressed_size: payload_len,
        });
        total_compressed += payload_len;
        total_original += seg.original_length;
    }

    Ok(CompressionStats {
        original_size: total_original,
        compressed_size: total_compressed,
        segment_count: segments.len(),
        segments: seg_stats,
    })
}

pub fn decompress<R: Read, W: Write>(input: &mut R, out: &mut W) -> Result<DecompressionStats, String> {
    // Header
    let mut magic = [0u8; 4];
    input.read_exact(&mut magic).r()?;
    if &magic != MAGIC {
        return Err(format!("not a CNEF file (magic: {:?})", magic));
    }
    let mut ver = [0u8; 1];
    input.read_exact(&mut ver).r()?;
    if ver[0] != VERSION {
        return Err(format!("unsupported CNEF version {}", ver[0]));
    }

    let original_file_size = read_u64_le(input)?;
    let segment_count = read_u32_le(input)? as usize;

    let mut reconstructed_size: u64 = 0;

    for _ in 0..segment_count {
        let mut type_buf = [0u8; 1];
        input.read_exact(&mut type_buf).r()?;
        let seg_type = SegmentType::from_u8(type_buf[0])?;

        let _original_offset = read_u64_le(input)?;
        let original_length = read_u64_le(input)?;
        let compressed_length = read_u64_le(input)? as usize;

        let raw_meta = if seg_type.has_raw_meta() {
            Some(RawPixelsMeta {
                width: read_u32_le(input)?,
                height: read_u32_le(input)?,
                bits_per_sample: read_u8(input)?,
                huff_select: read_u8(input)?,
                split_row: read_u32_le(input)?,
                initial_predictors: [
                    read_i32_le(input)?,
                    read_i32_le(input)?,
                    read_i32_le(input)?,
                    read_i32_le(input)?,
                ],
            })
        } else {
            None
        };

        let mut payload = vec![0u8; compressed_length];
        input.read_exact(&mut payload).r()?;

        match seg_type {
            SegmentType::Zstd => {
                let decompressed = zstd_decompress(&payload)?;
                out.write_all(&decompressed).w()?;
                reconstructed_size += decompressed.len() as u64;
            }
            SegmentType::JpegJxl => {
                let jpeg_bytes = jxl::decode_jpeg(&payload)?;
                let needed = original_length as usize;
                if jpeg_bytes.len() != needed {
                    return Err(format!(
                        "JPEG reconstruction size {} != expected {}",
                        jpeg_bytes.len(),
                        needed,
                    ));
                }
                out.write_all(&jpeg_bytes).w()?;
                reconstructed_size += needed as u64;
            }
            SegmentType::RawPixelsJxl | SegmentType::RawPixelsBiasedJxl => {
                let m = raw_meta.unwrap();
                let needed = original_length as usize;

                // Biased segments prefix the payload with an i32 bias to undo.
                let (bias, body) = if seg_type == SegmentType::RawPixelsBiasedJxl {
                    if payload.len() < 4 {
                        return Err("biased raw segment: payload too short".into());
                    }
                    (i32::from_le_bytes(payload[..4].try_into().unwrap()), &payload[4..])
                } else {
                    (0, payload.as_slice())
                };

                let (jxl_data, tail) = split_jxl_payload(body);
                let decoded = jxl::decode_pixels(jxl_data, m.width, m.height)?;
                let pixels: Vec<u16> = if bias != 0 {
                    decoded.iter().map(|&p| (p as i32 + bias) as u16).collect()
                } else {
                    decoded
                };

                let strip = reconstruct_raw_strip(
                    &pixels,
                    m.width as usize,
                    m.height as usize,
                    m.bits_per_sample as u32,
                    m.huff_select as usize,
                    [
                        [m.initial_predictors[0], m.initial_predictors[1]],
                        [m.initial_predictors[2], m.initial_predictors[3]],
                    ],
                    m.split_row as usize,
                    tail,
                    needed,
                )?;
                out.write_all(&strip).w()?;
                reconstructed_size += strip.len() as u64;
            }
            SegmentType::RawUncompressedJxl => {
                let m = raw_meta.unwrap();
                let needed = original_length as usize;
                let height = m.height as usize;

                let (jxl_data, _tail) = split_jxl_payload(&payload);
                let pixels = jxl::decode_pixels(jxl_data, m.width, m.height)?;

                let layout = packed::Layout::detect(needed, m.width as usize, height, m.bits_per_sample as u32)
                    .ok_or("uncompressed segment: irregular row layout")?;
                let strip = packed::pack(&pixels, m.width as usize, height, layout);
                if strip.len() != needed {
                    return Err(format!(
                        "uncompressed repack size {} != expected {needed}",
                        strip.len()
                    ));
                }
                out.write_all(&strip).w()?;
                reconstructed_size += strip.len() as u64;
            }
        }
    }

    if reconstructed_size != original_file_size {
        return Err(format!(
            "reconstructed size {reconstructed_size} != expected {original_file_size}"
        ));
    }

    Ok(DecompressionStats {
        original_size: original_file_size,
    })
}

#[derive(Debug)]
pub struct CompressionStats {
    pub original_size: u64,
    pub compressed_size: u64,
    pub segment_count: usize,
    pub segments: Vec<SegmentStats>,
}

#[derive(Debug)]
pub struct SegmentStats {
    pub seg_type: SegmentType,
    pub original_size: u64,
    pub compressed_size: u64,
}

#[derive(Debug)]
pub struct DecompressionStats {
    pub original_size: u64,
}

#[derive(Clone, Copy)]
enum RegionKind {
    RawStrip,
    Jpeg,
}

/// Re-encode decoded raw pixels back into the Nikon bitstream and fit it to
/// exactly `needed` bytes — the inverse of the RawStrip decode in `compress`.
///
/// The original strip is `needed` bytes. We splice the preserved `tail` (the
/// final partial byte + any trailing data) over the re-encoded body, then
/// truncate-or-pad the body to fill the rest. Truncation matters for lossy
/// streams whose pixel count slightly exceeds what the bitstream encodes: the
/// decoder over-runs into end-of-stream padding, so the re-encode is a few
/// bytes too long, but its leading `needed` bytes still match the original
/// exactly. Both `compress` (as a round-trip self-check) and `decompress` go
/// through here so they cannot disagree.
fn reconstruct_raw_strip(
    pixels: &[u16],
    width: usize,
    height: usize,
    bps: u32,
    huff_select: usize,
    predictors: [[i32; 2]; 2],
    split_row: usize,
    tail: &[u8],
    needed: usize,
) -> Result<Vec<u8>, String> {
    let reencoded =
        nikon_lossless::encode(pixels, width, height, bps, huff_select, predictors, split_row)?;
    if tail.len() > needed {
        return Err("raw strip tail longer than strip length".into());
    }
    let body_len = needed - tail.len();
    let mut out = Vec::with_capacity(needed);
    let copy = reencoded.len().min(body_len);
    out.extend_from_slice(&reencoded[..copy]);
    out.resize(body_len, 0); // pad if the re-encode came up short
    out.extend_from_slice(tail);
    Ok(out)
}

fn read_range<R: Read + Seek>(reader: &mut R, offset: u64, length: usize) -> Result<Vec<u8>, String> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek to {offset}: {e}"))?;
    let mut buf = vec![0u8; length];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("read {length} bytes at {offset}: {e}"))?;
    Ok(buf)
}

fn zstd_compress(data: &[u8]) -> Result<Vec<u8>, String> {
    zstd::encode_all(std::io::Cursor::new(data), ZSTD_LEVEL)
        .map_err(|e| format!("zstd compress: {e}"))
}

fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    zstd::decode_all(std::io::Cursor::new(data)).map_err(|e| format!("zstd decompress: {e}"))
}

fn read_u8<R: Read>(r: &mut R) -> Result<u8, String> {
    let mut buf = [0u8; 1];
    r.read_exact(&mut buf).r()?;
    Ok(buf[0])
}

fn read_u32_le<R: Read>(r: &mut R) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).r()?;
    Ok(u32::from_le_bytes(buf))
}

fn read_i32_le<R: Read>(r: &mut R) -> Result<i32, String> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).r()?;
    Ok(i32::from_le_bytes(buf))
}

fn read_u64_le<R: Read>(r: &mut R) -> Result<u64, String> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf).r()?;
    Ok(u64::from_le_bytes(buf))
}

trait IoResultExt<T> {
    fn r(self) -> Result<T, String>;
    fn w(self) -> Result<T, String>;
}

impl<T> IoResultExt<T> for Result<T, std::io::Error> {
    fn r(self) -> Result<T, String> {
        self.map_err(|e| format!("read error: {e}"))
    }
    fn w(self) -> Result<T, String> {
        self.map_err(|e| format!("write error: {e}"))
    }
}
