use std::io::{Read, Seek, SeekFrom};

use crate::tiff::*;

#[derive(Debug, Clone)]
pub struct NefChunks {
    pub file_size: u64,
    pub tiff_view: TiffView,
    pub raw_strip: RawStrip,
    pub jpegs: Vec<JpegChunk>,
}

#[derive(Debug, Clone)]
pub struct NikonLosslessMeta {
    pub huff_select: usize,
    pub initial_predictors: [[i32; 2]; 2],
    pub split_row: usize,
}

#[derive(Debug, Clone)]
pub struct RawStrip {
    pub offset: u64,
    pub length: u64,
    pub width: u32,
    pub height: u32,
    pub bits_per_sample: u32,
    pub compression: u32,
    pub is_jpeg_xs: bool,
}

#[derive(Debug, Clone)]
pub struct JpegChunk {
    pub offset: u64,
    pub length: u32,
    pub width: u32,
    pub height: u32,
    pub label: JpegLabel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegLabel {
    Thumbnail,
    PreviewImage,
    JpgFromRaw,
}

pub fn scan_nef<R: Read + Seek>(reader: &mut R) -> Result<NefChunks, String> {
    let file_size = reader
        .seek(std::io::SeekFrom::End(0))
        .map_err(|e| format!("seek to end: {e}"))?;
    reader
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|e| format!("seek to start: {e}"))?;

    let (main, ifd0_offset) = TiffView::parse_header(reader, 0)?;
    let ifd0 = main.read_ifd(reader, ifd0_offset)?;

    let mut jpegs = Vec::new();

    // IFD0 thumbnail (160x120)
    if let Some(jpeg) = jpeg_from_inline_pointers(&main, reader, &ifd0)? {
        jpegs.push(JpegChunk {
            offset: jpeg.offset,
            length: jpeg.length,
            width: jpeg.width,
            height: jpeg.height,
            label: JpegLabel::Thumbnail,
        });
    }

    // SubIFDs — find the raw strip and JpgFromRaw
    let mut raw_strip: Option<RawStrip> = None;
    let mut best_jpeg: Option<JpegChunk> = None;

    if let Some(subifds_entry) = main.find_entry(&ifd0, TAG_SUB_IFDS) {
        let subifd_offsets = main.entry_long_array(reader, subifds_entry)?;
        for offset in subifd_offsets {
            let entries = match main.read_ifd(reader, offset) {
                Ok(e) => e,
                Err(_) => continue,
            };

            let compression = main
                .find_entry(&entries, TAG_COMPRESSION)
                .and_then(|e| main.entry_scalar(e))
                .unwrap_or(1);

            let width = main
                .find_entry(&entries, TAG_IMAGE_WIDTH)
                .and_then(|e| main.entry_scalar(e))
                .unwrap_or(0);
            let height = main
                .find_entry(&entries, TAG_IMAGE_LENGTH)
                .and_then(|e| main.entry_scalar(e))
                .unwrap_or(0);

            if compression == COMPRESSION_NIKON_LOSSLESS || compression == 1 {
                // Raw strip
                let strip_offset = main
                    .find_entry(&entries, TAG_STRIP_OFFSETS)
                    .and_then(|e| main.entry_scalar(e))
                    .unwrap_or(0) as u64;
                let strip_length = main
                    .find_entry(&entries, TAG_STRIP_BYTE_COUNTS)
                    .and_then(|e| main.entry_scalar(e))
                    .unwrap_or(0) as u64;
                let bps = main
                    .find_entry(&entries, TAG_BITS_PER_SAMPLE)
                    .and_then(|e| main.entry_scalar(e))
                    .unwrap_or(14);

                if strip_length > 0 {
                    let is_bigger = raw_strip
                        .as_ref()
                        .map_or(true, |r| width * height > r.width * r.height);
                    if is_bigger {
                        let is_jpeg_xs = detect_jpeg_xs(reader, strip_offset)?;
                        raw_strip = Some(RawStrip {
                            offset: strip_offset,
                            length: strip_length,
                            width,
                            height,
                            bits_per_sample: bps,
                            compression,
                            is_jpeg_xs,
                        });
                    }
                }
            } else if compression == COMPRESSION_JPEG_OLD_STYLE || compression == COMPRESSION_JPEG
            {
                if let Some(jpeg) = jpeg_from_inline_pointers(&main, reader, &entries)? {
                    let bigger = match &best_jpeg {
                        Some(existing)
                            if existing.width * existing.height >= jpeg.width * jpeg.height =>
                        {
                            false
                        }
                        _ => true,
                    };
                    if bigger {
                        best_jpeg = Some(JpegChunk {
                            offset: jpeg.offset,
                            length: jpeg.length,
                            width: jpeg.width,
                            height: jpeg.height,
                            label: JpegLabel::JpgFromRaw,
                        });
                    }
                }
            }
        }
    }

    if let Some(j) = best_jpeg {
        jpegs.push(j);
    }

    // MakerNote → PreviewIFD → PreviewImage (640x424)
    if let Some(exif_entry) = main.find_entry(&ifd0, TAG_EXIF_IFD) {
        let exif_offset = main
            .entry_scalar(exif_entry)
            .ok_or("ExifIFD pointer not scalar")?;
        if let Some(preview) = find_preview_image(&main, reader, exif_offset)? {
            jpegs.push(JpegChunk {
                offset: preview.offset,
                length: preview.length,
                width: preview.width,
                height: preview.height,
                label: JpegLabel::PreviewImage,
            });
        }
    }

    let raw_strip = raw_strip.ok_or("NEF: no raw data strip found")?;

    Ok(NefChunks {
        file_size,
        tiff_view: main,
        raw_strip,
        jpegs,
    })
}

fn jpeg_from_inline_pointers<R: Read + Seek>(
    view: &TiffView,
    reader: &mut R,
    entries: &[RawEntry],
) -> Result<Option<EmbeddedJpeg>, String> {
    let offset = view
        .find_entry(entries, TAG_JPEG_INTERCHANGE_FORMAT)
        .and_then(|e| view.entry_scalar(e));
    let length = view
        .find_entry(entries, TAG_JPEG_INTERCHANGE_FORMAT_LENGTH)
        .and_then(|e| view.entry_scalar(e));
    let (Some(offset), Some(length)) = (offset, length) else {
        return Ok(None);
    };
    if length == 0 {
        return Ok(None);
    }
    let abs_offset = view.abs_from_ifd_offset(offset) as u64;
    let (width, height) = read_jpeg_sof(reader, abs_offset as usize, length as usize)?;
    Ok(Some(EmbeddedJpeg {
        offset: abs_offset,
        length,
        width,
        height,
    }))
}

// JPEG XS start-of-codestream (SOC) + capabilities (CAP) marker pair
const JPEG_XS_SOC_CAP: [u8; 4] = [0xFF, 0x10, 0xFF, 0x50];

const MAKERNOTE_HEADER: &[u8; 6] = b"Nikon\0";
const MAKERNOTE_HEADER_SIZE: usize = 10;

// Nikon MakerNote tag IDs for the metadata blob containing Huffman
// table selection, predictor init values, linearization curve, etc.
const TAG_NEFMETA2: u16 = 0x0096;
const TAG_NEFMETA1: u16 = 0x008C;

fn detect_jpeg_xs<R: Read + Seek>(reader: &mut R, offset: u64) -> Result<bool, String> {
    reader
        .seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek to strip: {e}"))?;
    let mut buf = [0u8; 4];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("read strip header: {e}"))?;
    Ok(buf == JPEG_XS_SOC_CAP)
}

pub fn read_nikon_lossless_meta<R: Read + Seek>(
    reader: &mut R,
    chunks: &NefChunks,
) -> Result<NikonLosslessMeta, String> {
    let main = &chunks.tiff_view;
    let (_, ifd0_offset) = TiffView::parse_header(reader, 0)?;
    let ifd0 = main.read_ifd(reader, ifd0_offset)?;

    let exif_entry = main.find_entry(&ifd0, TAG_EXIF_IFD).ok_or("no ExifIFD")?;
    let exif_offset = main.entry_scalar(exif_entry).ok_or("ExifIFD not scalar")?;
    let exif_ifd = main.read_ifd(reader, exif_offset)?;

    let makernote_entry = main
        .find_entry(&exif_ifd, TAG_MAKERNOTE)
        .ok_or("no MakerNote")?;
    let makernote_rel = main.decode_u32(&makernote_entry.value_bytes);
    let makernote_abs = main.abs_from_ifd_offset(makernote_rel);

    reader
        .seek(SeekFrom::Start(makernote_abs as u64))
        .map_err(|e| format!("seek MakerNote: {e}"))?;
    let mut header = [0u8; MAKERNOTE_HEADER_SIZE];
    reader
        .read_exact(&mut header)
        .map_err(|e| format!("read MakerNote header: {e}"))?;
    if &header[..6] != MAKERNOTE_HEADER {
        return Err("not a Nikon MakerNote".into());
    }

    let embedded_base = makernote_abs + MAKERNOTE_HEADER_SIZE;
    let (embedded, nikon_ifd_offset) = TiffView::parse_header(reader, embedded_base)?;
    let nikon_ifd = embedded.read_ifd(reader, nikon_ifd_offset)?;

    let meta_entry = embedded
        .find_entry(&nikon_ifd, TAG_NEFMETA2)
        .or_else(|| embedded.find_entry(&nikon_ifd, TAG_NEFMETA1))
        .ok_or("no NefMeta tag")?;

    let meta_byte_size = meta_entry.byte_size();
    let meta_offset = if meta_byte_size > 4 {
        embedded.abs_from_ifd_offset(embedded.decode_u32(&meta_entry.value_bytes))
    } else {
        return Err("NefMeta too small".into());
    };
    let meta_bytes = main.read_bytes(reader, meta_offset as u64, meta_byte_size)?;

    parse_nikon_meta(&meta_bytes, &embedded, chunks.raw_strip.bits_per_sample)
}

// NefMeta version bytes that select the Huffman table and curve format.
// Names follow rawspeed/LibRaw conventions derived from dcraw.
const NEFMETA_VER_LOSSLESS: u8 = 0x46;
const NEFMETA_VER_LOSSY_CURVE: u8 = 0x44;
const NEFMETA_V0_SKIP_2110: u8 = 0x49;
const NEFMETA_V1_SKIP_2110: u8 = 0x58;
// Lossy sub-variants that enable the linearization curve + split row
const NEFMETA_V1_CURVE_NORMAL: u8 = 0x20;
const NEFMETA_V1_CURVE_QUARTERED: u8 = 0x40;
// Fixed byte offset of the split-row field within the NefMeta blob
const NEFMETA_SPLIT_ROW_OFFSET: usize = 562;

// Extra header bytes skipped for certain firmware versions.
// This block contains encrypted WB/calibration data and precedes the
// predictor init values.
const NEFMETA_ENCRYPTED_BLOCK_SIZE: usize = 2110;

// Huffman table indexing: tables 0-2 are 12-bit (lossy, lossy-after-split,
// lossless), tables 3-5 are the 14-bit equivalents in the same order.
const HUFF_TABLE_LOSSLESS_12: usize = 2;
const HUFF_TABLE_14BIT_OFFSET: usize = 3;

fn parse_nikon_meta(
    meta: &[u8],
    embedded: &TiffView,
    bps: u32,
) -> Result<NikonLosslessMeta, String> {
    if meta.len() < 10 {
        return Err("NefMeta too short".into());
    }

    let v0 = meta[0];
    let v1 = meta[1];

    let mut pos = 2usize;
    if (v0 == NEFMETA_V0_SKIP_2110 || v1 == NEFMETA_V1_SKIP_2110)
        && pos + NEFMETA_ENCRYPTED_BLOCK_SIZE + 8 <= meta.len()
    {
        pos += NEFMETA_ENCRYPTED_BLOCK_SIZE;
    }

    let mut huff_select: usize = 0;
    if v0 == NEFMETA_VER_LOSSLESS {
        huff_select = HUFF_TABLE_LOSSLESS_12;
    }
    if bps == 14 {
        huff_select += HUFF_TABLE_14BIT_OFFSET;
    }

    if pos + 8 > meta.len() {
        return Err("NefMeta: predictor data truncated".into());
    }

    let read_u16 = |offset: usize| -> u16 { embedded.decode_u16(&meta[offset..offset + 2]) };

    // Read order matches rawspeed: pUp[0][0], pUp[1][0], pUp[0][1], pUp[1][1]
    let val0 = read_u16(pos) as i32;
    let val1 = read_u16(pos + 2) as i32;
    let val2 = read_u16(pos + 4) as i32;
    let val3 = read_u16(pos + 6) as i32;
    pos += 8;

    let mut split_row = 0usize;

    if pos + 2 <= meta.len() {
        let csize = read_u16(pos) as usize;
        if v0 == NEFMETA_VER_LOSSY_CURVE
            && (v1 == NEFMETA_V1_CURVE_NORMAL || v1 == NEFMETA_V1_CURVE_QUARTERED)
            && csize > 1
        {
            if NEFMETA_SPLIT_ROW_OFFSET + 2 <= meta.len() {
                split_row = read_u16(NEFMETA_SPLIT_ROW_OFFSET) as usize;
            }
        }
    }

    Ok(NikonLosslessMeta {
        huff_select,
        initial_predictors: [[val0, val2], [val1, val3]],
        split_row,
    })
}

fn find_preview_image<R: Read + Seek>(
    main: &TiffView,
    reader: &mut R,
    exif_ifd_offset: u32,
) -> Result<Option<EmbeddedJpeg>, String> {
    let exif_ifd = main.read_ifd(reader, exif_ifd_offset)?;
    let makernote_entry = match main.find_entry(&exif_ifd, TAG_MAKERNOTE) {
        Some(e) => e,
        None => return Ok(None),
    };
    if makernote_entry.count <= 4 {
        return Ok(None);
    }
    let makernote_rel = main.decode_u32(&makernote_entry.value_bytes);
    let makernote_abs = main.abs_from_ifd_offset(makernote_rel);

    reader
        .seek(std::io::SeekFrom::Start(makernote_abs as u64))
        .map_err(|e| format!("MakerNote seek: {e}"))?;
    let mut header = [0u8; MAKERNOTE_HEADER_SIZE];
    if reader.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if &header[..6] != MAKERNOTE_HEADER {
        return Ok(None);
    }

    let embedded_base = makernote_abs + MAKERNOTE_HEADER_SIZE;
    let (embedded, nikon_ifd_offset) = TiffView::parse_header(reader, embedded_base)?;
    let nikon_ifd = embedded.read_ifd(reader, nikon_ifd_offset)?;

    let preview_ifd_entry = match embedded.find_entry(&nikon_ifd, TAG_NIKON_PREVIEW_IFD) {
        Some(e) => e,
        None => return Ok(None),
    };
    let preview_ifd_offset = embedded
        .entry_scalar(preview_ifd_entry)
        .ok_or("Nikon PreviewIFD pointer not scalar")?;

    let preview_ifd = embedded.read_ifd(reader, preview_ifd_offset)?;
    jpeg_from_inline_pointers(&embedded, reader, &preview_ifd)
}
