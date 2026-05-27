use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use nef_compactor::nef::{read_nikon_lossless_meta, scan_nef};
use nef_compactor::nikon_lossless;
use nef_compactor::tiff::COMPRESSION_NIKON_LOSSLESS;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: dump-raw <input.NEF> <output.raw>");
        eprintln!("Writes decoded 14-bit Bayer pixels as u16le to output file.");
        eprintln!("Also writes a deinterleaved version as <output>_4ch.raw");
        std::process::exit(1);
    }

    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);

    let mut file = std::fs::File::open(&input).expect("open NEF");
    let chunks = scan_nef(&mut file).expect("scan NEF");

    if chunks.raw_strip.compression != COMPRESSION_NIKON_LOSSLESS {
        eprintln!("not a Nikon lossless compressed NEF");
        std::process::exit(1);
    }

    let meta = read_nikon_lossless_meta(&mut file, &chunks).expect("read meta");

    file.seek(SeekFrom::Start(chunks.raw_strip.offset)).unwrap();
    let mut compressed = vec![0u8; chunks.raw_strip.length as usize];
    file.read_exact(&mut compressed).unwrap();

    let w = chunks.raw_strip.width as usize;
    let h = chunks.raw_strip.height as usize;
    let pixels = nikon_lossless::decode(
        &compressed,
        w,
        h,
        chunks.raw_strip.bits_per_sample,
        meta.huff_select,
        meta.initial_predictors,
        meta.split_row,
    )
    .expect("decode")
    .pixels;

    // Write flat mosaic
    let bytes: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
    std::fs::File::create(&output)
        .expect("create output")
        .write_all(&bytes)
        .expect("write");

    eprintln!(
        "mosaic: {}x{} {}bps → {} ({} bytes)",
        w, h, chunks.raw_strip.bits_per_sample,
        output.display(),
        bytes.len(),
    );
    eprintln!("nikon compressed: {} bytes ({:.1}%)",
        chunks.raw_strip.length,
        chunks.raw_strip.length as f64 / bytes.len() as f64 * 100.0,
    );

    // Write deinterleaved 4-channel version
    let half_w = w / 2;
    let half_h = h / 2;
    let mut deint = vec![0u16; half_w * half_h * 4];
    for row in 0..half_h {
        for col in 0..half_w {
            let dst = (row * half_w + col) * 4;
            deint[dst] = pixels[(row * 2) * w + (col * 2)];
            deint[dst + 1] = pixels[(row * 2) * w + (col * 2 + 1)];
            deint[dst + 2] = pixels[(row * 2 + 1) * w + (col * 2)];
            deint[dst + 3] = pixels[(row * 2 + 1) * w + (col * 2 + 1)];
        }
    }

    let out_4ch = output.with_extension("4ch.raw");
    let bytes_4ch: Vec<u8> = deint.iter().flat_map(|p| p.to_le_bytes()).collect();
    std::fs::File::create(&out_4ch)
        .expect("create 4ch output")
        .write_all(&bytes_4ch)
        .expect("write 4ch");

    eprintln!(
        "deinterleaved: {}x{} 4ch interleaved → {} ({} bytes)",
        half_w, half_h, out_4ch.display(), bytes_4ch.len(),
    );

    // Also write planar version (4 consecutive planes for ffmpeg yuva444p16le)
    let out_planar = output.with_extension("planar.raw");
    let n = half_w * half_h;
    let mut planar_bytes: Vec<u8> = Vec::with_capacity(n * 4 * 2);
    for ch in 0..4 {
        for i in 0..n {
            planar_bytes.extend_from_slice(&deint[i * 4 + ch].to_le_bytes());
        }
    }
    std::fs::File::create(&out_planar)
        .expect("create planar output")
        .write_all(&planar_bytes)
        .expect("write planar");

    eprintln!(
        "deinterleaved: {}x{} 4ch planar → {} ({} bytes)",
        half_w, half_h, out_planar.display(), planar_bytes.len(),
    );
}
