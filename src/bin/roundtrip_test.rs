use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use nef_compactor::nef::{read_nikon_lossless_meta, scan_nef};
use nef_compactor::nikon_lossless;
use nef_compactor::tiff::COMPRESSION_NIKON_LOSSLESS;

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

    let meta = read_nikon_lossless_meta(&mut file, &chunks)?;
    println!(
        "  huff_select={}, split_row={}, preds={:?}",
        meta.huff_select, meta.split_row, meta.initial_predictors,
    );

    file.seek(SeekFrom::Start(chunks.raw_strip.offset))
        .map_err(|e| format!("seek raw: {e}"))?;
    let mut compressed = vec![0u8; chunks.raw_strip.length as usize];
    file.read_exact(&mut compressed)
        .map_err(|e| format!("read raw: {e}"))?;

    let width = chunks.raw_strip.width as usize;
    let height = chunks.raw_strip.height as usize;
    let bps = chunks.raw_strip.bits_per_sample;

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
