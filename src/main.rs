use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: nef-compactor <input.NEF> [output.cnef]");
        std::process::exit(1);
    }

    let input = PathBuf::from(&args[1]);
    let _output = if args.len() > 2 {
        PathBuf::from(&args[2])
    } else {
        input.with_extension("cnef")
    };

    let mut file = std::fs::File::open(&input).expect("open input");
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
