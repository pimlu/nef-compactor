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

use crate::nef::{NefChunks, NikonLosslessMeta};
use crate::nikon_lossless;
use crate::tiff::COMPRESSION_NIKON_LOSSLESS;

const MAGIC: &[u8; 4] = b"CNEF";
const VERSION: u8 = 1;

const ZSTD_LEVEL: i32 = 19;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentType {
    Zstd = 1,
    RawPixels = 2,
}

impl SegmentType {
    fn from_u8(v: u8) -> Result<Self, String> {
        match v {
            1 => Ok(Self::Zstd),
            2 => Ok(Self::RawPixels),
            _ => Err(format!("unknown segment type {v}")),
        }
    }
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
    lossless_meta: Option<&NikonLosslessMeta>,
    out: &mut W,
) -> Result<CompressionStats, String> {
    let mut regions: Vec<(u64, u64, RegionKind)> = Vec::new();

    // Collect all chunk regions
    if chunks.raw_strip.compression == COMPRESSION_NIKON_LOSSLESS && lossless_meta.is_some() {
        regions.push((
            chunks.raw_strip.offset,
            chunks.raw_strip.length,
            RegionKind::RawStrip,
        ));
    } else {
        regions.push((
            chunks.raw_strip.offset,
            chunks.raw_strip.length,
            RegionKind::Blob,
        ));
    }
    for jpeg in &chunks.jpegs {
        regions.push((jpeg.offset, jpeg.length as u64, RegionKind::Blob));
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
                let meta = lossless_meta.unwrap();
                let compressed_raw = read_range(nef, offset, length as usize)?;
                let width = chunks.raw_strip.width as usize;
                let height = chunks.raw_strip.height as usize;
                let bps = chunks.raw_strip.bits_per_sample;

                let pixels = nikon_lossless::decode(
                    &compressed_raw,
                    width,
                    height,
                    bps,
                    meta.huff_select,
                    meta.initial_predictors,
                    meta.split_row,
                )?;

                // Compress raw pixels with zstd (JXL replacement comes later)
                let pixel_bytes = pixels_to_bytes(&pixels);
                let compressed_pixels = zstd_compress(&pixel_bytes)?;

                segments.push(Segment {
                    seg_type: SegmentType::RawPixels,
                    original_offset: offset,
                    original_length: length,
                    payload: compressed_pixels,
                    raw_meta: Some(RawPixelsMeta {
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
                    }),
                });
            }
            RegionKind::Blob => {
                let data = read_range(nef, offset, length as usize)?;
                segments.push(Segment {
                    seg_type: SegmentType::Zstd,
                    original_offset: offset,
                    original_length: length,
                    payload: zstd_compress(&data)?,
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

        total_compressed += seg.payload.len() as u64;
        total_original += seg.original_length;
    }

    Ok(CompressionStats {
        original_size: total_original,
        compressed_size: total_compressed,
        segment_count: segments.len(),
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

        let raw_meta = if seg_type == SegmentType::RawPixels {
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
                let decompressed = zstd_decompress(&payload, original_length as usize)?;
                out.write_all(&decompressed).w()?;
                reconstructed_size += decompressed.len() as u64;
            }
            SegmentType::RawPixels => {
                let m = raw_meta.unwrap();
                let pixel_bytes = zstd_decompress(&payload, 0)?;
                let pixels = bytes_to_pixels(&pixel_bytes);

                let nikon_compressed = nikon_lossless::encode(
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
                )?;

                // The Nikon encoder may produce fewer bytes than original due
                // to trailing padding in the original strip. Pad to match.
                let needed = original_length as usize;
                if nikon_compressed.len() > needed {
                    return Err(format!(
                        "re-encoded raw strip {} bytes > original {} bytes",
                        nikon_compressed.len(),
                        needed,
                    ));
                }
                out.write_all(&nikon_compressed).w()?;
                if nikon_compressed.len() < needed {
                    let pad = vec![0u8; needed - nikon_compressed.len()];
                    out.write_all(&pad).w()?;
                }
                reconstructed_size += needed as u64;
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
}

#[derive(Debug)]
pub struct DecompressionStats {
    pub original_size: u64,
}

#[derive(Clone, Copy)]
enum RegionKind {
    RawStrip,
    Blob,
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

fn zstd_decompress(data: &[u8], _expected_size: usize) -> Result<Vec<u8>, String> {
    zstd::decode_all(std::io::Cursor::new(data)).map_err(|e| format!("zstd decompress: {e}"))
}

fn pixels_to_bytes(pixels: &[u16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(pixels.len() * 2);
    for &p in pixels {
        bytes.extend_from_slice(&p.to_le_bytes());
    }
    bytes
}

fn bytes_to_pixels(bytes: &[u8]) -> Vec<u16> {
    bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
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
