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
        "  {} segments, {} → {} bytes ({:.1}%)",
        stats.segment_count,
        stats.original_size,
        cnef_size,
        ratio * 100.0,
    );
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
}
