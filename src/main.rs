use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};

use lexopt::prelude::*;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }

    match args[1].as_str() {
        "compress" | "c" => cmd_compress_main(&args[2..]),
        "decompress" | "d" => cmd_decompress_main(&args[2..]),
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
    eprintln!("  compress   [-R] [-j N] [-e EFFORT] [--dry-run] [--skip-verify] [--rm] [-v] <file|dir> [output]");
    eprintln!("             -R: compress all NEFs in a directory");
    eprintln!("             effort: 1 (fastest) to 10 (slowest), default 3");
    eprintln!("             verify roundtrip by default; --skip-verify to disable");
    eprintln!("             --rm: remove original .NEF after verified write (incompatible with --skip-verify)");
    eprintln!("             -v: show per-segment breakdown");
    eprintln!("  decompress [-R] [-j N] [--rm] <file.CNEF|dir> [output]");
    eprintln!("             -R: decompress all CNEFs in a directory");
    eprintln!("             --rm: remove original .CNEF after writing .NEF");
    eprintln!("  info       <input.NEF>");
    std::process::exit(1);
}

fn die(msg: impl std::fmt::Display) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1);
}

fn collect_files(dir: &Path, extensions: &[&str]) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(e) => {
                eprintln!("warning: cannot read {}: {e}", d.display());
                continue;
            }
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if extensions.iter().any(|&e| e == ext) {
                    result.push(p);
                }
            }
        }
    }
    result.sort();
    result
}

/// Raw extensions we operate on, and their compressed counterparts, are
/// related by a leading 'C'/'c': NEF↔CNEF, nef↔cnef, NRW↔CNRW, nrw↔cnrw.
const RAW_EXTENSIONS: &[&str] = &["NEF", "nef", "NRW", "nrw"];
const COMPRESSED_EXTENSIONS: &[&str] = &["CNEF", "cnef", "CNRW", "cnrw"];

/// Compressed-side extension for a raw input: prepend 'C'/'c' to the input
/// extension, matching its case (NEF→CNEF, nrw→cnrw).
fn compressed_extension(input: &Path) -> String {
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("NEF");
    let c = if ext.starts_with(|c: char| c.is_ascii_lowercase()) {
        'c'
    } else {
        'C'
    };
    format!("{c}{ext}")
}

/// Inverse of `compressed_extension`: strip the leading 'C'/'c'
/// (CNEF→NEF, cnrw→nrw).
fn decompressed_extension(input: &Path) -> String {
    let ext = input.extension().and_then(|e| e.to_str()).unwrap_or("NEF");
    ext.strip_prefix(|c: char| c == 'C' || c == 'c')
        .unwrap_or(ext)
        .to_string()
}

// ─── Compress ────────────────────────────────────────────────────────────────

struct CompressOpts {
    jobs: usize,
    effort: i64,
    dry_run: bool,
    skip_verify: bool,
    verbose: bool,
    remove_originals: bool,
    recursive: bool,
    input: PathBuf,
    output: Option<PathBuf>,
}

fn parse_compress_args(args: &[String]) -> CompressOpts {
    let mut jobs = 1usize;
    let mut effort = 3i64;
    let mut dry_run = false;
    let mut skip_verify = false;
    let mut verbose = false;
    let mut remove_originals = false;
    let mut recursive = false;
    let mut positional = Vec::new();

    let mut parser = lexopt::Parser::from_args(args);
    while let Some(arg) = parser.next().unwrap_or_else(|e| die(e)) {
        match arg {
            Short('j') => jobs = parser.value().unwrap_or_else(|e| die(e))
                .parse().unwrap_or_else(|e| die(format!("-j: {e}"))),
            Short('e') => effort = parser.value().unwrap_or_else(|e| die(e))
                .parse().unwrap_or_else(|e| die(format!("-e: {e}"))),
            Short('v') | Long("verbose") => verbose = true,
            Short('R') => recursive = true,
            Long("dry-run") => dry_run = true,
            Long("skip-verify") => skip_verify = true,
            Long("rm") => remove_originals = true,
            Value(val) => positional.push(val.into()),
            _ => die(arg.unexpected()),
        }
    }

    if positional.is_empty() {
        eprintln!(
            "usage: nef-compactor compress [-R] [-j N] [-e EFFORT] [--dry-run] [--skip-verify] [--rm] [-v] <file|dir> [output]"
        );
        std::process::exit(1);
    }

    if remove_originals && skip_verify {
        die("--rm cannot be used with --skip-verify");
    }

    CompressOpts {
        jobs,
        effort: effort.clamp(1, 10),
        dry_run,
        skip_verify,
        verbose,
        remove_originals,
        recursive,
        input: positional.remove(0),
        output: positional.into_iter().next(),
    }
}

fn write_and_sync(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(data)?;
    f.sync_all()?;
    Ok(())
}

fn cmd_compress_main(args: &[String]) {
    let opts = parse_compress_args(args);

    if opts.input.is_dir() {
        if !opts.recursive {
            die(format!("{} is a directory, use -R to compress all NEFs in it", opts.input.display()));
        }
        cmd_compress_dir(&opts);
    } else {
        if opts.recursive {
            die(format!("-R requires a directory, got file: {}", opts.input.display()));
        }
        let out_path = opts
            .output
            .unwrap_or_else(|| opts.input.with_extension(compressed_extension(&opts.input)));
        let name = opts.input.file_name().unwrap().to_string_lossy();
        match compress_one(&opts.input, &name, opts.effort, opts.skip_verify) {
            Ok(result) => {
                if !opts.dry_run {
                    if let Some(ref data) = result.cnef_data {
                        write_and_sync(&out_path, data).expect("write output");
                        if opts.remove_originals {
                            std::fs::remove_file(&opts.input).expect("remove original");
                        }
                    }
                }
                print_compress_result(&result, opts.verbose, opts.dry_run);
            }
            Err(e) => {
                eprintln!("{}: {e}", opts.input.display());
                std::process::exit(1);
            }
        }
    }
}

struct CompressResult {
    input_name: String,
    original_size: u64,
    cnef_size: u64,
    cnef_data: Option<Vec<u8>>,
    segments: Vec<nef_compactor::cnef::SegmentStats>,
}

fn print_compress_result(r: &CompressResult, verbose: bool, dry_run: bool) {
    let ratio = r.cnef_size as f64 / r.original_size as f64 * 100.0;
    let target = if dry_run { "(dry-run)" } else { "" };
    println!("{} → {target}({:.1}%)", r.input_name, ratio);
    if verbose {
        for seg in &r.segments {
            let label = match seg.seg_type {
                nef_compactor::cnef::SegmentType::Zstd => "zstd",
                nef_compactor::cnef::SegmentType::RawPixelsJxl => "raw→jxl",
                nef_compactor::cnef::SegmentType::JpegJxl => "jpeg→jxl",
                nef_compactor::cnef::SegmentType::RawUncompressedJxl => "raw(unc)→jxl",
                nef_compactor::cnef::SegmentType::RawPixelsBiasedJxl => "raw(bias)→jxl",
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
}

/// Compress a single NEF to memory. Does NOT write to disk — caller decides.
fn compress_one(
    input: &Path,
    display_name: &str,
    effort: i64,
    skip_verify: bool,
) -> Result<CompressResult, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut nef_file = std::fs::File::open(input).map_err(|e| format!("open: {e}"))?;
    let chunks = nef_compactor::nef::scan_nef(&mut nef_file)?;

    if chunks.raw_strip.is_jpeg_xs {
        return Err("HE/HE* NEF not supported (HE/HE* already lossy encodes the raw with JPEG XS)".into());
    }

    let rs = &chunks.raw_strip;
    let pixels = rs.width as u64 * rs.height as u64;

    // sNEF "small RAW" stores 3 bytes/pixel of YCbCr instead of a Bayer stream;
    // the lossless tag lies. Detect it by size and skip with a clear note.
    if rs.length == pixels * 3 {
        return Err("sNEF (YCbCr small RAW) not supported".into());
    }

    // A real Nikon Huffman strip is always smaller than the packed samples. A
    // strip at or above packed size is uncompressed raw (compression=1, or on
    // some bodies mislabelled as Nikon Compressed) — recompressed by unpacking
    // the samples and feeding them through the same JPEG XL path.
    let packed_size = pixels * rs.bits_per_sample as u64 / 8;
    let raw_source = if rs.compression == nef_compactor::tiff::COMPRESSION_NIKON_LOSSLESS
        && rs.length < packed_size
    {
        nef_compactor::cnef::RawSource::Lossless(
            nef_compactor::nef::read_nikon_lossless_meta(&mut nef_file, &chunks)
                .map_err(|e| format!("Nikon raw linearization metadata not found ({e})"))?,
        )
    } else {
        // Uncompressed: we repack row by row, so the strip must divide evenly
        // into rows (it always does in practice; guard to be safe).
        let h = rs.height as u64;
        if h == 0 || rs.length % h != 0 {
            return Err(format!(
                "uncompressed raw with irregular layout ({} bytes / {} rows)",
                rs.length, rs.height
            ));
        }
        nef_compactor::cnef::RawSource::Uncompressed
    };

    let original_size = chunks.file_size;

    let mut cnef_buf = Vec::new();
    let stats = nef_compactor::cnef::compress(
        &mut nef_file,
        &chunks,
        &raw_source,
        effort,
        &mut cnef_buf,
    )?;
    let cnef_size = cnef_buf.len() as u64;

    if !skip_verify {
        let mut cursor = std::io::Cursor::new(&cnef_buf);
        let mut reconstructed = Vec::new();
        nef_compactor::cnef::decompress(&mut cursor, &mut reconstructed)?;

        nef_file
            .seek(SeekFrom::Start(0))
            .map_err(|e| format!("seek: {e}"))?;
        let mut original = Vec::with_capacity(original_size as usize);
        nef_file
            .read_to_end(&mut original)
            .map_err(|e| format!("read original: {e}"))?;

        if original != reconstructed {
            let first_diff = original
                .iter()
                .zip(reconstructed.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(original.len().min(reconstructed.len()));
            return Err(format!(
                "verification failed: mismatch at byte {first_diff} (original {} bytes, reconstructed {} bytes)",
                original.len(), reconstructed.len(),
            ));
        }
    }

    Ok(CompressResult {
        input_name: display_name.to_string(),
        original_size,
        cnef_size,
        cnef_data: Some(cnef_buf),
        segments: stats.segments,
    })
}

fn cmd_compress_dir(opts: &CompressOpts) {
    let dir = &opts.input;
    let out_dir = opts.output.as_deref().unwrap_or(dir);
    if !opts.dry_run && !out_dir.exists() {
        std::fs::create_dir_all(out_dir).expect("create output directory");
    }

    let files = collect_files(dir, RAW_EXTENSIONS);

    if files.is_empty() {
        eprintln!("no .NEF/.nef/.NRW/.nrw files found in {}", dir.display());
        std::process::exit(1);
    }

    let out_paths: Vec<Option<PathBuf>> = files
        .iter()
        .map(|f| {
            if opts.dry_run {
                None
            } else {
                let rel = f.strip_prefix(dir).unwrap_or(f.as_path());
                let mut dest = out_dir.join(rel);
                dest.set_extension(compressed_extension(f));
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).expect("create output subdirectory");
                }
                Some(dest)
            }
        })
        .collect();

    let display_names: Vec<String> = files
        .iter()
        .map(|f| {
            f.strip_prefix(dir)
                .unwrap_or(f.as_path())
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let effort = opts.effort;
    let skip_verify = opts.skip_verify;
    let verbose = opts.verbose;
    let dry_run = opts.dry_run;
    let remove_originals = opts.remove_originals;

    // Shared state for in-order streaming output
    let state = StreamState::new(files.len());

    let n = files.len();

    if opts.jobs <= 1 {
        for i in 0..n {
            let result = compress_one(&files[i], &display_names[i], effort, skip_verify);
            state.publish(i, result);
            state.drain(&files, &display_names, &out_paths, verbose, dry_run, remove_originals);
        }
    } else {
        let next_idx = Mutex::new(0usize);

        std::thread::scope(|scope| {
            let drainer = scope.spawn(|| {
                state.drain_blocking(n, &files, &display_names, &out_paths, verbose, dry_run, remove_originals);
            });

            for _ in 0..opts.jobs {
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

                    let result = compress_one(&files[idx], &display_names[idx], effort, skip_verify);
                    state.publish(idx, result);
                });
            }

            drainer.join().unwrap();
        });
    }

    state.print_summary(files.len());
}

struct StreamState {
    inner: Mutex<StreamInner>,
    cond: Condvar,
}

struct StreamInner {
    slots: Vec<Option<Result<CompressResult, String>>>,
    next_print: usize,
    compressed: u64,
    failed: u64,
    total_original: u64,
    total_cnef: u64,
}

impl StreamState {
    fn new(n: usize) -> Self {
        StreamState {
            inner: Mutex::new(StreamInner {
                slots: (0..n).map(|_| None).collect(),
                next_print: 0,
                compressed: 0,
                failed: 0,
                total_original: 0,
                total_cnef: 0,
            }),
            cond: Condvar::new(),
        }
    }

    fn publish(&self, idx: usize, result: Result<CompressResult, String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.slots[idx] = Some(result);
        self.cond.notify_all();
    }

    fn drain(
        &self,
        files: &[PathBuf],
        display_names: &[String],
        out_paths: &[Option<PathBuf>],
        verbose: bool,
        dry_run: bool,
        remove_originals: bool,
    ) {
        let mut inner = self.inner.lock().unwrap();
        while inner.next_print < inner.slots.len() {
            let idx = inner.next_print;
            let Some(result) = inner.slots[idx].take() else {
                break;
            };
            inner.next_print += 1;

            match result {
                Ok(r) => {
                    if !dry_run {
                        if let (Some(data), Some(path)) = (&r.cnef_data, &out_paths[idx]) {
                            if let Err(e) = write_and_sync(path, data) {
                                eprintln!("{}: write failed ({e})", r.input_name);
                                inner.failed += 1;
                                continue;
                            }
                            if remove_originals {
                                if let Err(e) = std::fs::remove_file(&files[idx]) {
                                    eprintln!("{}: remove failed ({e})", r.input_name);
                                }
                            }
                        }
                    }
                    inner.compressed += 1;
                    inner.total_original += r.original_size;
                    inner.total_cnef += r.cnef_size;
                    print_compress_result(&r, verbose, dry_run);
                }
                Err(e) => {
                    eprintln!("{}: skipped ({e})", display_names[idx]);
                    inner.failed += 1;
                }
            }
        }
    }

    fn drain_blocking(
        &self,
        n: usize,
        files: &[PathBuf],
        display_names: &[String],
        out_paths: &[Option<PathBuf>],
        verbose: bool,
        dry_run: bool,
        remove_originals: bool,
    ) {
        loop {
            self.drain(files, display_names, out_paths, verbose, dry_run, remove_originals);
            let inner = self.inner.lock().unwrap();
            if inner.next_print >= n {
                return;
            }
            let _unused = self.cond.wait(inner).unwrap();
        }
    }

    fn print_summary(&self, total_files: usize) {
        let inner = self.inner.lock().unwrap();
        let total_ratio = if inner.total_original > 0 {
            inner.total_cnef as f64 / inner.total_original as f64 * 100.0
        } else {
            0.0
        };

        println!();
        println!(
            "Summary: {} compressed, {} skipped, {} total",
            inner.compressed, inner.failed, total_files,
        );
        if inner.compressed > 0 {
            println!(
                "  total: {} → {} bytes ({:.1}%)",
                inner.total_original, inner.total_cnef, total_ratio,
            );
        }
    }
}

// ─── Decompress ──────────────────────────────────────────────────────────────

fn cmd_decompress_main(args: &[String]) {
    let mut recursive = false;
    let mut remove_originals = false;
    let mut jobs = 1usize;
    let mut positional: Vec<PathBuf> = Vec::new();

    let mut parser = lexopt::Parser::from_args(args);
    while let Some(arg) = parser.next().unwrap_or_else(|e| die(e)) {
        match arg {
            Short('R') => recursive = true,
            Short('j') => jobs = parser.value().unwrap_or_else(|e| die(e))
                .parse().unwrap_or_else(|e| die(format!("-j: {e}"))),
            Long("rm") => remove_originals = true,
            Value(val) => positional.push(val.into()),
            _ => die(arg.unexpected()),
        }
    }

    if positional.is_empty() {
        eprintln!("usage: nef-compactor decompress [-R] [-j N] [--rm] <input.CNEF|dir> [output]");
        std::process::exit(1);
    }

    let input = positional.remove(0);
    let output = positional.into_iter().next();

    if input.is_dir() {
        if !recursive {
            die(format!("{} is a directory, use -R to decompress all CNEFs in it", input.display()));
        }
        cmd_decompress_dir(&input, output.as_deref(), jobs, remove_originals);
    } else {
        if recursive {
            die(format!("-R requires a directory, got file: {}", input.display()));
        }
        let out_path = output.unwrap_or_else(|| input.with_extension(decompressed_extension(&input)));
        let name = input.file_name().unwrap().to_string_lossy();
        match decompress_one(&input, &name) {
            Ok(r) => {
                write_and_sync(&out_path, &r.nef_data).expect("write output NEF");
                if remove_originals {
                    std::fs::remove_file(&input).expect("remove original CNEF");
                }
                println!(
                    "{} → {} ({} bytes)",
                    r.input_name,
                    out_path.file_name().unwrap().to_string_lossy(),
                    r.original_size,
                );
            }
            Err(e) => {
                eprintln!("{}: {e}", input.display());
                std::process::exit(1);
            }
        }
    }
}

struct DecompressResult {
    input_name: String,
    original_size: u64,
    nef_data: Vec<u8>,
}

fn decompress_one(input: &Path, display_name: &str) -> Result<DecompressResult, String> {
    let mut cnef_file =
        std::fs::File::open(input).map_err(|e| format!("open: {e}"))?;
    let mut nef_buf = Vec::new();

    let stats = nef_compactor::cnef::decompress(&mut cnef_file, &mut nef_buf)?;

    Ok(DecompressResult {
        input_name: display_name.to_string(),
        original_size: stats.original_size,
        nef_data: nef_buf,
    })
}

fn cmd_decompress_dir(dir: &Path, output: Option<&Path>, jobs: usize, remove_originals: bool) {
    let out_dir = output.unwrap_or(dir);

    let files = collect_files(dir, COMPRESSED_EXTENSIONS);

    if files.is_empty() {
        eprintln!("no .CNEF/.cnef/.CNRW/.cnrw files found in {}", dir.display());
        std::process::exit(1);
    }

    let out_paths: Vec<PathBuf> = files
        .iter()
        .map(|f| {
            let rel = f.strip_prefix(dir).unwrap_or(f.as_path());
            let mut dest = out_dir.join(rel);
            dest.set_extension(decompressed_extension(f));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).expect("create output subdirectory");
            }
            dest
        })
        .collect();

    let display_names: Vec<String> = files
        .iter()
        .map(|f| {
            f.strip_prefix(dir)
                .unwrap_or(f.as_path())
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let n = files.len();
    let state = DecompressStreamState::new(n);

    if jobs <= 1 {
        for i in 0..n {
            let result = decompress_one(&files[i], &display_names[i]);
            state.publish(i, result);
            state.drain(&files, &display_names, &out_paths, remove_originals);
        }
    } else {
        let next_idx = Mutex::new(0usize);

        std::thread::scope(|scope| {
            let drainer = scope.spawn(|| {
                state.drain_blocking(n, &files, &display_names, &out_paths, remove_originals);
            });

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

                    let result = decompress_one(&files[idx], &display_names[idx]);
                    state.publish(idx, result);
                });
            }

            drainer.join().unwrap();
        });
    }

    let inner = state.inner.lock().unwrap();
    println!(
        "\nSummary: {} decompressed, {} failed, {} total",
        inner.succeeded, inner.failed, n,
    );
}

struct DecompressStreamState {
    inner: Mutex<DecompressStreamInner>,
    cond: Condvar,
}

struct DecompressStreamInner {
    slots: Vec<Option<Result<DecompressResult, String>>>,
    next_print: usize,
    succeeded: u64,
    failed: u64,
}

impl DecompressStreamState {
    fn new(n: usize) -> Self {
        DecompressStreamState {
            inner: Mutex::new(DecompressStreamInner {
                slots: (0..n).map(|_| None).collect(),
                next_print: 0,
                succeeded: 0,
                failed: 0,
            }),
            cond: Condvar::new(),
        }
    }

    fn publish(&self, idx: usize, result: Result<DecompressResult, String>) {
        let mut inner = self.inner.lock().unwrap();
        inner.slots[idx] = Some(result);
        self.cond.notify_all();
    }

    fn drain(
        &self,
        files: &[PathBuf],
        display_names: &[String],
        out_paths: &[PathBuf],
        remove_originals: bool,
    ) {
        let mut inner = self.inner.lock().unwrap();
        while inner.next_print < inner.slots.len() {
            let idx = inner.next_print;
            let Some(result) = inner.slots[idx].take() else {
                break;
            };
            inner.next_print += 1;

            match result {
                Ok(r) => {
                    if let Err(e) = write_and_sync(&out_paths[idx], &r.nef_data) {
                        eprintln!("{}: write failed ({e})", r.input_name);
                        inner.failed += 1;
                        continue;
                    }
                    if remove_originals {
                        if let Err(e) = std::fs::remove_file(&files[idx]) {
                            eprintln!("{}: remove failed ({e})", r.input_name);
                        }
                    }
                    inner.succeeded += 1;
                    println!(
                        "{} → {} ({} bytes)",
                        r.input_name,
                        out_paths[idx].file_name().unwrap().to_string_lossy(),
                        r.original_size,
                    );
                }
                Err(e) => {
                    eprintln!("{}: failed ({e})", display_names[idx]);
                    inner.failed += 1;
                }
            }
        }
    }

    fn drain_blocking(
        &self,
        n: usize,
        files: &[PathBuf],
        display_names: &[String],
        out_paths: &[PathBuf],
        remove_originals: bool,
    ) {
        loop {
            self.drain(files, display_names, out_paths, remove_originals);
            let inner = self.inner.lock().unwrap();
            if inner.next_print >= n {
                return;
            }
            let _unused = self.cond.wait(inner).unwrap();
        }
    }
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
        let decode_result = nef_compactor::nikon_lossless::decode(
            &compressed,
            w,
            h,
            chunks.raw_strip.bits_per_sample,
            meta.huff_select,
            meta.initial_predictors,
            meta.split_row,
        )
        .expect("decode");
        let pixels = decode_result.pixels;

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
