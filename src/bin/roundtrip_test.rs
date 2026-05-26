use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use nef_compactor::nef::{scan_nef, NefChunks};
use nef_compactor::nikon_lossless;
use nef_compactor::tiff::*;

fn read_nef_compression_metadata<R: Read + Seek>(
    reader: &mut R,
    chunks: &NefChunks,
) -> Result<CompressionMeta, String> {
    let main = &chunks.tiff_view;
    let (_, ifd0_offset) = TiffView::parse_header(reader, 0)?;
    let ifd0 = main.read_ifd(reader, ifd0_offset)?;

    let subifds_entry = main
        .find_entry(&ifd0, TAG_SUB_IFDS)
        .ok_or("no SubIFDs")?;
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

        if compression != COMPRESSION_NIKON_LOSSLESS {
            continue;
        }

        let width = main
            .find_entry(&entries, TAG_IMAGE_WIDTH)
            .and_then(|e| main.entry_scalar(e))
            .unwrap_or(0);
        let height = main
            .find_entry(&entries, TAG_IMAGE_LENGTH)
            .and_then(|e| main.entry_scalar(e))
            .unwrap_or(0);

        if width != chunks.raw_strip.width || height != chunks.raw_strip.height {
            continue;
        }

        // Find the metadata tag (NefMeta2 = 0x0096, NefMeta1 = 0x008C)
        // These live in the ExifIFD's MakerNote's Nikon IFD.
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
        let mut header = [0u8; 10];
        reader
            .read_exact(&mut header)
            .map_err(|e| format!("read MakerNote header: {e}"))?;
        if &header[..6] != b"Nikon\0" {
            return Err("not a Nikon MakerNote".into());
        }

        let embedded_base = makernote_abs + 10;
        let (embedded, nikon_ifd_offset) = TiffView::parse_header(reader, embedded_base)?;
        let nikon_ifd = embedded.read_ifd(reader, nikon_ifd_offset)?;

        // NefMeta2 (0x0096) or NefMeta1 (0x008C)
        let meta_entry = embedded
            .find_entry(&nikon_ifd, 0x0096)
            .or_else(|| embedded.find_entry(&nikon_ifd, 0x008C))
            .ok_or("no NefMeta tag")?;

        let meta_offset = if meta_entry.count > 4 {
            embedded.abs_from_ifd_offset(embedded.decode_u32(&meta_entry.value_bytes))
        } else {
            return Err("NefMeta too small".into());
        };
        let meta_len = meta_entry.count as usize;
        let meta_bytes = main.read_bytes(reader, meta_offset as u64, meta_len)?;

        let bps = chunks.raw_strip.bits_per_sample;
        return parse_nikon_meta(&meta_bytes, &embedded, bps);
    }

    Err("no Nikon lossless compressed SubIFD found".into())
}

struct CompressionMeta {
    huff_select: usize,
    initial_predictors: [[i32; 2]; 2],
    split_row: usize,
}

fn parse_nikon_meta(
    meta: &[u8],
    embedded: &TiffView,
    bps: u32,
) -> Result<CompressionMeta, String> {
    if meta.len() < 10 {
        return Err("NefMeta too short".into());
    }

    let v0 = meta[0];
    let v1 = meta[1];

    let mut pos = 2usize;
    if v0 == 73 || v1 == 88 {
        pos += 2110;
    }

    let mut huff_select: usize = 0;
    if v0 == 70 {
        huff_select = 2;
    }
    if bps == 14 {
        huff_select += 3;
    }

    if pos + 8 > meta.len() {
        return Err("NefMeta: predictor data truncated".into());
    }

    let read_u16 = |offset: usize| -> u16 { embedded.decode_u16(&meta[offset..offset + 2]) };

    // Read order matches rawspeed: pUp[0][0], pUp[1][0], pUp[0][1], pUp[1][1]
    // pUp[row&1][col&1] — first index is row parity, second is column parity
    let val0 = read_u16(pos) as i32;
    let val1 = read_u16(pos + 2) as i32;
    let val2 = read_u16(pos + 4) as i32;
    let val3 = read_u16(pos + 6) as i32;
    pos += 8;

    let mut split_row = 0usize;

    if pos + 2 <= meta.len() {
        let csize = read_u16(pos) as usize;

        if v0 == 68 && (v1 == 32 || v1 == 64) && csize > 1 {
            if 562 + 2 <= meta.len() {
                split_row = read_u16(562) as usize;
            }
        }
    }

    Ok(CompressionMeta {
        huff_select,
        // pUp[0] = [val0, val2], pUp[1] = [val1, val3]
        initial_predictors: [[val0, val2], [val1, val3]],
        split_row,
    })
}

fn test_roundtrip(path: &Path) -> Result<(), String> {
    println!("Testing: {}", path.display());

    let mut file =
        std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let chunks = scan_nef(&mut file)?;

    if chunks.raw_strip.compression != COMPRESSION_NIKON_LOSSLESS {
        println!(
            "  skipping: compression={} (not Nikon lossless)",
            chunks.raw_strip.compression
        );
        return Ok(());
    }

    println!(
        "  raw: {}x{} {}bps, {} bytes at {:#x}",
        chunks.raw_strip.width,
        chunks.raw_strip.height,
        chunks.raw_strip.bits_per_sample,
        chunks.raw_strip.length,
        chunks.raw_strip.offset,
    );

    let meta = read_nef_compression_metadata(&mut file, &chunks)?;
    println!(
        "  huff_select={}, split_row={}, preds={:?}",
        meta.huff_select, meta.split_row, meta.initial_predictors,
    );

    // Read the compressed raw strip
    file.seek(SeekFrom::Start(chunks.raw_strip.offset))
        .map_err(|e| format!("seek raw: {e}"))?;
    let mut compressed = vec![0u8; chunks.raw_strip.length as usize];
    file.read_exact(&mut compressed)
        .map_err(|e| format!("read raw: {e}"))?;

    let width = chunks.raw_strip.width as usize;
    let height = chunks.raw_strip.height as usize;
    let bps = chunks.raw_strip.bits_per_sample;

    // Decode
    let pixels = nikon_lossless::decode(
        &compressed,
        width,
        height,
        bps,
        meta.huff_select,
        meta.initial_predictors,
        meta.split_row,
    )?;
    println!("  decoded {} pixels", pixels.len());

    // Re-encode
    let recompressed = nikon_lossless::encode(
        &pixels,
        width,
        height,
        bps,
        meta.huff_select,
        meta.initial_predictors,
        meta.split_row,
    )?;
    println!(
        "  re-encoded: {} bytes (original: {} bytes)",
        recompressed.len(),
        compressed.len()
    );

    // Compare
    let match_len = compressed.len().min(recompressed.len());
    let mut first_diff = None;
    for i in 0..match_len {
        if compressed[i] != recompressed[i] {
            first_diff = Some(i);
            break;
        }
    }

    if let Some(pos) = first_diff {
        let orig = compressed[pos];
        let re = recompressed[pos];
        println!("  MISMATCH at byte {pos}: original=0x{orig:02x} re-encoded=0x{re:02x}");

        let context_start = pos.saturating_sub(4);
        let context_end = (pos + 5).min(match_len);
        println!(
            "    original:    {:02x?}",
            &compressed[context_start..context_end]
        );
        println!(
            "    re-encoded:  {:02x?}",
            &recompressed[context_start..context_end]
        );

        return Err(format!("bitstream mismatch at byte {pos}"));
    }

    if compressed.len() != recompressed.len() {
        // Check if the difference is just trailing zeros (padding)
        let longer = if compressed.len() > recompressed.len() {
            &compressed[match_len..]
        } else {
            &recompressed[match_len..]
        };
        let all_zero = longer.iter().all(|&b| b == 0);
        if all_zero {
            println!(
                "  OK (bitstream matches; {} trailing zero bytes differ)",
                longer.len()
            );
        } else {
            return Err(format!(
                "length mismatch: original={} re-encoded={} (non-zero trailing bytes)",
                compressed.len(),
                recompressed.len()
            ));
        }
    } else {
        println!("  OK — bit-perfect roundtrip!");
    }

    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: roundtrip-test <file.NEF | directory>");
        std::process::exit(1);
    }

    let path = PathBuf::from(&args[1]);
    let mut files = Vec::new();

    if path.is_dir() {
        for entry in std::fs::read_dir(&path).expect("read directory") {
            let entry = entry.expect("dir entry");
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) == Some("NEF") {
                files.push(p);
            }
        }
        files.sort();
    } else {
        files.push(path);
    }

    if files.is_empty() {
        eprintln!("no NEF files found");
        std::process::exit(1);
    }

    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;

    for file in &files {
        match test_roundtrip(file) {
            Ok(()) => passed += 1,
            Err(e) => {
                if e.contains("skipping") {
                    skipped += 1;
                } else {
                    eprintln!("  FAIL: {e}");
                    failed += 1;
                }
            }
        }
        println!();
    }

    println!("Results: {passed} passed, {failed} failed, {skipped} skipped");
    if failed > 0 {
        std::process::exit(1);
    }
}
