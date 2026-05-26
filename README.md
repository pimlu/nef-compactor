# nef-compactor

Lossless compression for Nikon NEF files. Produces `.CNEF` files that are 20-40% smaller than the originals and decompress back to byte-identical NEFs.

Uses JPEG XL for raw sensor data and embedded JPEGs, zstd for metadata. No patent-encumbered codecs — HE/HE\* NEFs (which use JPEG XS internally) are detected and skipped.

## How it works

A NEF is a TIFF container holding a few large chunks: raw sensor data (~20-25 MB), a camera-rendered JPEG (~1-3 MB), thumbnail JPEGs, and TIFF/EXIF/MakerNote metadata. nef-compactor splits the file at chunk boundaries and compresses each piece with the best available codec:

| Chunk | Strategy | Typical ratio |
|---|---|---|
| Raw strip (14-bit Bayer) | Nikon lossless decode → Bayer deinterleave → JXL lossless | 79-96% |
| Embedded JPEGs | JXL JPEG recompression (bit-perfect) | 71-91% |
| Metadata skeleton | zstd | 40-90% |

On decompression, the raw strip is re-encoded back to Nikon's lossless format using the same Huffman tables and prediction scheme, producing an identical bitstream. The output is byte-for-byte identical to the original NEF.

Compression ratio depends heavily on ISO — low-ISO images (clean data, high spatial correlation) compress much better than high-ISO images (noisy, less redundancy).

## Usage

```
# Preview compression ratios without writing anything
nef-compactor compress --dry-run -e3 -j8 ~/Photos/

# Compress a directory (8 threads, effort 3)
nef-compactor compress -e3 -j8 ~/Photos/

# Compress and remove originals after verified write
nef-compactor compress -e3 -j8 --rm ~/Photos/

# Verbose output showing per-segment breakdown
nef-compactor compress -e3 -j8 -v ~/Photos/

# Decompress back to original NEF
nef-compactor decompress photo.CNEF

# Inspect a NEF file's structure
nef-compactor info photo.NEF
```

### Flags

| Flag | Description |
|---|---|
| `-e N` | JXL effort 1 (fastest) to 10 (slowest), default 3 |
| `-j N` | Number of parallel threads |
| `--dry-run` | Show ratios without writing files |
| `--skip-verify` | Skip roundtrip verification (faster, less safe) |
| `--rm` | Remove original `.NEF` after verified, fsynced write. Incompatible with `--skip-verify` |
| `-v` | Show per-segment compression breakdown |

## Building

```
cargo build --release
```

Requires a C compiler for the vendored libjxl.

## Source layout

| File | Purpose |
|---|---|
| `src/main.rs` | CLI, batch processing, streaming output |
| `src/nef.rs` | NEF/TIFF scanner, chunk extraction, JPEG XS detection |
| `src/tiff.rs` | Minimal TIFF/IFD parser (including Nikon MakerNote) |
| `src/nikon_lossless.rs` | Nikon lossless codec (decoder + encoder) |
| `src/jxl.rs` | JPEG XL wrapper (raw pixel encoding, JPEG recompression) |
| `src/cnef.rs` | CNEF container format (compress/decompress/segment table) |
