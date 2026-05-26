use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }

    match args[1].as_str() {
        "compress" | "c" => cmd_compress_main(&args[2..]),
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
    eprintln!("  compress   [-j N] [-e EFFORT] [--dry-run] [--skip-verify] <input | dir> [output]");
    eprintln!("             effort: 1 (fastest) to 10 (slowest), default 3");
    eprintln!("             verify roundtrip by default; --skip-verify to disable");
    eprintln!("  decompress <input.cnef> [output.NEF]");
    eprintln!("  info       <input.NEF>");
    std::process::exit(1);
}

// ─── Compress ────────────────────────────────────────────────────────────────

struct CompressOpts {
    jobs: usize,
    effort: i64,
    dry_run: bool,
    skip_verify: bool,
    input: PathBuf,
    output: Option<PathBuf>,
}

fn parse_num_after_flag(args: &[String], i: &mut usize, flag: &str) -> Option<i64> {
    *i += 1;
    args.get(*i).and_then(|s| s.parse().ok()).or_else(|| {
        eprintln!("{flag} requires a number");
        std::process::exit(1);
    })
}

fn parse_compress_args(args: &[String]) -> CompressOpts {
    let mut jobs = 1usize;
    let mut effort = 3i64;
    let mut dry_run = false;
    let mut skip_verify = false;
    let mut positional = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-j" => jobs = parse_num_after_flag(args, &mut i, "-j").unwrap() as usize,
            "-e" => effort = parse_num_after_flag(args, &mut i, "-e").unwrap(),
            "--dry-run" => dry_run = true,
            "--skip-verify" => skip_verify = true,
            _ if args[i].starts_with("-j") => {
                jobs = args[i][2..].parse().unwrap_or_else(|_| {
                    eprintln!("-j requires a number");
                    std::process::exit(1);
                });
            }
            _ if args[i].starts_with("-e") => {
                effort = args[i][2..].parse().unwrap_or_else(|_| {
                    eprintln!("-e requires a number");
                    std::process::exit(1);
                });
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }

    if positional.is_empty() {
        eprintln!("usage: nef-compactor compress [-j N] [-e EFFORT] [--dry-run] <input> [output]");
        std::process::exit(1);
    }

    CompressOpts {
        jobs,
        effort: effort.clamp(1, 10),
        dry_run,
        skip_verify,
        input: PathBuf::from(&positional[0]),
        output: positional.get(1).map(PathBuf::from),
    }
}

fn cmd_compress_main(args: &[String]) {
    let opts = parse_compress_args(args);

    if opts.input.is_dir() {
        cmd_compress_dir(&opts);
    } else {
        let output = opts
            .output
            .unwrap_or_else(|| opts.input.with_extension("cnef"));
        match compress_one(&opts.input, if opts.dry_run { None } else { Some(&output) }, opts.effort, opts.skip_verify) {
            Ok(result) => print_compress_result(&result),
            Err(e) => {
                eprintln!("{}: {e}", opts.input.display());
                std::process::exit(1);
            }
        }
    }
}

struct CompressResult {
    input_name: String,
    output_name: String,
    original_size: u64,
    cnef_size: u64,
    segments: Vec<nef_compactor::cnef::SegmentStats>,
}

fn print_compress_result(r: &CompressResult) {
    let ratio = r.cnef_size as f64 / r.original_size as f64 * 100.0;
    println!("{} → {} ({:.1}%)", r.input_name, r.output_name, ratio);
    for seg in &r.segments {
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

fn compress_one(input: &Path, output: Option<&Path>, effort: i64, skip_verify: bool) -> Result<CompressResult, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut nef_file = std::fs::File::open(input).map_err(|e| format!("open: {e}"))?;
    let chunks = nef_compactor::nef::scan_nef(&mut nef_file)?;

    let lossless_meta = if chunks.raw_strip.compression
        == nef_compactor::tiff::COMPRESSION_NIKON_LOSSLESS
    {
        match nef_compactor::nef::read_nikon_lossless_meta(&mut nef_file, &chunks) {
            Ok(m) => Some(m),
            Err(e) => {
                eprintln!(
                    "  warning: can't read lossless meta ({e}), raw strip will use zstd"
                );
                None
            }
        }
    } else {
        None
    };

    let original_size = chunks.file_size;

    let mut cnef_buf = Vec::new();
    let stats = nef_compactor::cnef::compress(
        &mut nef_file,
        &chunks,
        lossless_meta.as_ref(),
        effort,
        &mut cnef_buf,
    )?;
    let cnef_size = cnef_buf.len() as u64;

    if !skip_verify {
        let mut cursor = std::io::Cursor::new(&cnef_buf);
        let mut reconstructed = Vec::new();
        nef_compactor::cnef::decompress(&mut cursor, &mut reconstructed)?;

        nef_file.seek(SeekFrom::Start(0)).map_err(|e| format!("seek: {e}"))?;
        let mut original = Vec::with_capacity(original_size as usize);
        nef_file.read_to_end(&mut original).map_err(|e| format!("read original: {e}"))?;

        if original != reconstructed {
            let first_diff = original.iter().zip(reconstructed.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(original.len().min(reconstructed.len()));
            return Err(format!(
                "verification failed: mismatch at byte {first_diff} (original {} bytes, reconstructed {} bytes)",
                original.len(), reconstructed.len(),
            ));
        }
    }

    if let Some(out_path) = output {
        std::fs::write(out_path, &cnef_buf).map_err(|e| format!("write output: {e}"))?;
    }

    let input_name = input.file_name().unwrap().to_string_lossy().to_string();
    let output_name = if let Some(p) = output {
        p.file_name().unwrap().to_string_lossy().to_string()
    } else {
        "(dry-run)".to_string()
    };

    Ok(CompressResult {
        input_name,
        output_name,
        original_size,
        cnef_size,
        segments: stats.segments,
    })
}

fn cmd_compress_dir(opts: &CompressOpts) {
    let dir = &opts.input;
    let out_dir = opts.output.as_deref().unwrap_or(dir);
    if !opts.dry_run && !out_dir.exists() {
        std::fs::create_dir_all(out_dir).expect("create output directory");
    }

    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read directory")
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension().and_then(|e| e.to_str()) == Some("NEF") {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    files.sort();

    if files.is_empty() {
        eprintln!("no NEF files found in {}", dir.display());
        std::process::exit(1);
    }

    // Build (input, output) pairs in order
    let tasks: Vec<(PathBuf, Option<PathBuf>)> = files
        .iter()
        .map(|f| {
            let stem = f.file_stem().unwrap().to_string_lossy();
            let out = if opts.dry_run {
                None
            } else {
                Some(out_dir.join(format!("{stem}.cnef")))
            };
            (f.clone(), out)
        })
        .collect();

    let effort = opts.effort;
    let skip_verify = opts.skip_verify;
    let results: Vec<Result<CompressResult, String>> = if opts.jobs <= 1 {
        tasks
            .iter()
            .map(|(input, output)| compress_one(input, output.as_deref(), effort, skip_verify))
            .collect()
    } else {
        parallel_compress(&tasks, opts.jobs, effort, skip_verify)
    };

    let mut compressed = 0u64;
    let mut failed = 0u64;
    let mut total_original = 0u64;
    let mut total_cnef = 0u64;

    for (i, result) in results.into_iter().enumerate() {
        match result {
            Ok(r) => {
                total_original += r.original_size;
                total_cnef += r.cnef_size;
                compressed += 1;
                print_compress_result(&r);
            }
            Err(e) => {
                let name = files[i].file_name().unwrap().to_string_lossy();
                eprintln!("{name}: skipped ({e})");
                failed += 1;
            }
        }
    }

    let total_ratio = if total_original > 0 {
        total_cnef as f64 / total_original as f64 * 100.0
    } else {
        0.0
    };

    println!();
    println!(
        "Summary: {} compressed, {} skipped, {} total",
        compressed,
        failed,
        files.len(),
    );
    if compressed > 0 {
        println!(
            "  total: {} → {} bytes ({:.1}%)",
            total_original, total_cnef, total_ratio,
        );
    }
}

fn parallel_compress(
    tasks: &[(PathBuf, Option<PathBuf>)],
    jobs: usize,
    effort: i64,
    skip_verify: bool,
) -> Vec<Result<CompressResult, String>> {
    let n = tasks.len();
    let results: Vec<Mutex<Option<Result<CompressResult, String>>>> =
        (0..n).map(|_| Mutex::new(None)).collect();
    let next_idx = Mutex::new(0usize);

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| loop {
                let idx = {
                    let mut next = next_idx.lock().unwrap();
                    if *next >= n {
                        return;
                    }
                    let i = *next;
                    *next += 1;
                    i
                };

                let (input, output) = &tasks[idx];
                let result = compress_one(input, output.as_deref(), effort, skip_verify);
                *results[idx].lock().unwrap() = Some(result);
            });
        }
    });

    results
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}

// ─── Decompress ──────────────────────────────────────────────────────────────

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

// ─── Info ────────────────────────────────────────────────────────────────────

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
            &compressed,
            w,
            h,
            chunks.raw_strip.bits_per_sample,
            meta.huff_select,
            meta.initial_predictors,
            meta.split_row,
        )
        .expect("decode");

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
        let bits_needed = if max_abs == 0 {
            0
        } else {
            32 - max_abs.leading_zeros()
        };

        println!(
            "  G delta: range [{}, +{}], avg |delta|={:.1}, bits needed={}",
            min_delta,
            max_delta,
            sum_abs as f64 / count,
            bits_needed,
        );
    }
}
