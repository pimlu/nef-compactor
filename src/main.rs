use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }

    match args[1].as_str() {
        "compress" | "c" => {
            if args.len() < 3 {
                eprintln!("usage: nef-compactor compress <input.NEF> [output.cnef]");
                std::process::exit(1);
            }
            let input = PathBuf::from(&args[2]);
            let output = if args.len() > 3 {
                PathBuf::from(&args[3])
            } else {
                input.with_extension("cnef")
            };
            cmd_compress(&input, &output);
        }
        "decompress" | "d" => {
            if args.len() < 3 {
                eprintln!("usage: nef-compactor decompress <input.cnef> [output.NEF]");
                std::process::exit(1);
            }
            let input = PathBuf::from(&args[2]);
            let output = if args.len() > 3 {
                PathBuf::from(&args[3])
            } else {
                input.with_extension("NEF")
            };
            cmd_decompress(&input, &output);
        }
        "info" | "i" => {
            if args.len() < 3 {
                eprintln!("usage: nef-compactor info <input.NEF>");
                std::process::exit(1);
            }
            cmd_info(&PathBuf::from(&args[2]));
        }
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: nef-compactor <command> [args...]");
    eprintln!();
    eprintln!("commands:");
    eprintln!("  compress   <input.NEF> [output.cnef]   Compress NEF to CNEF");
    eprintln!("  decompress <input.cnef> [output.NEF]   Decompress CNEF to NEF");
    eprintln!("  info       <input.NEF>                 Show NEF chunk layout");
    std::process::exit(1);
}

fn cmd_compress(input: &PathBuf, output: &PathBuf) {
    let mut nef_file = std::fs::File::open(input).expect("open input NEF");
    let chunks = nef_compactor::nef::scan_nef(&mut nef_file).expect("scan NEF");

    let lossless_meta = if chunks.raw_strip.compression
        == nef_compactor::tiff::COMPRESSION_NIKON_LOSSLESS
    {
        Some(
            nef_compactor::nef::read_nikon_lossless_meta(&mut nef_file, &chunks)
                .expect("read compression metadata"),
        )
    } else {
        None
    };

    let mut out_file = std::fs::File::create(output).expect("create output CNEF");
    let stats = nef_compactor::cnef::compress(
        &mut nef_file,
        &chunks,
        lossless_meta.as_ref(),
        &mut out_file,
    )
    .expect("compress");

    let cnef_size = std::fs::metadata(output).expect("stat output").len();
    let ratio = cnef_size as f64 / stats.original_size as f64;

    println!(
        "{} → {}",
        input.file_name().unwrap().to_string_lossy(),
        output.file_name().unwrap().to_string_lossy(),
    );
    println!(
        "  total: {} → {} bytes ({:.1}%)",
        stats.original_size, cnef_size, ratio * 100.0,
    );
    for seg in &stats.segments {
        let label = match seg.seg_type {
            nef_compactor::cnef::SegmentType::Zstd => "zstd",
            nef_compactor::cnef::SegmentType::RawPixelsJxl => "raw→jxl",
            nef_compactor::cnef::SegmentType::JpegJxl => "jpeg→jxl",
        };
        let seg_ratio = if seg.original_size > 0 {
            seg.compressed_size as f64 / seg.original_size as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "    {:<10} {:>10} → {:>10} ({:.1}%)",
            label, seg.original_size, seg.compressed_size, seg_ratio,
        );
    }
}

fn cmd_decompress(input: &PathBuf, output: &PathBuf) {
    let mut cnef_file = std::fs::File::open(input).expect("open input CNEF");
    let mut out_file = std::fs::File::create(output).expect("create output NEF");

    let stats =
        nef_compactor::cnef::decompress(&mut cnef_file, &mut out_file).expect("decompress");

    println!(
        "{} → {} ({} bytes)",
        input.file_name().unwrap().to_string_lossy(),
        output.file_name().unwrap().to_string_lossy(),
        stats.original_size,
    );
}

fn cmd_info(input: &PathBuf) {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(input).expect("open input");
    let chunks = nef_compactor::nef::scan_nef(&mut file).expect("scan NEF");

    println!("NEF: {} bytes", chunks.file_size);
    println!(
        "  raw strip: offset={:#x} length={} ({}x{}, {}bps, compression={})",
        chunks.raw_strip.offset,
        chunks.raw_strip.length,
        chunks.raw_strip.width,
        chunks.raw_strip.height,
        chunks.raw_strip.bits_per_sample,
        chunks.raw_strip.compression,
    );
    for jpeg in &chunks.jpegs {
        println!(
            "  {:?}: offset={:#x} length={} ({}x{})",
            jpeg.label, jpeg.offset, jpeg.length, jpeg.width, jpeg.height,
        );
    }

    if chunks.raw_strip.compression == nef_compactor::tiff::COMPRESSION_NIKON_LOSSLESS {
        let meta =
            nef_compactor::nef::read_nikon_lossless_meta(&mut file, &chunks).expect("read meta");
        file.seek(SeekFrom::Start(chunks.raw_strip.offset)).unwrap();
        let mut compressed = vec![0u8; chunks.raw_strip.length as usize];
        file.read_exact(&mut compressed).unwrap();

        let w = chunks.raw_strip.width as usize;
        let h = chunks.raw_strip.height as usize;
        let pixels = nef_compactor::nikon_lossless::decode(
            &compressed, w, h, chunks.raw_strip.bits_per_sample,
            meta.huff_select, meta.initial_predictors, meta.split_row,
        ).expect("decode");

        let half_w = w / 2;
        let half_h = h / 2;
        let mut min_delta: i32 = 0;
        let mut max_delta: i32 = 0;
        let mut sum_abs: u64 = 0;

        for row in 0..half_h {
            for col in 0..half_w {
                let g1 = pixels[(row * 2) * w + (col * 2 + 1)] as i32;
                let g2 = pixels[(row * 2 + 1) * w + (col * 2)] as i32;
                let delta = g1 - g2;
                min_delta = min_delta.min(delta);
                max_delta = max_delta.max(delta);
                sum_abs += delta.unsigned_abs() as u64;
            }
        }

        let count = (half_w * half_h) as f64;
        let max_abs = max_delta.abs().max(min_delta.abs()) as u32;
        let bits_needed = if max_abs == 0 { 0 } else { 32 - max_abs.leading_zeros() };

        println!(
            "  G delta: range [{}, +{}], avg |delta|={:.1}, bits needed={}",
            min_delta, max_delta, sum_abs as f64 / count, bits_needed,
        );
    }
}
