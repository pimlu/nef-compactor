# nef-compactor — Lossless NEF Compression

A standalone tool for losslessly compressing Nikon NEF files into a compact `.cnef` format, with bit-perfect round-trip reconstruction.

## Motivation

Nikon's HE/HE★ NEF formats use TicoRAW (patent-encumbered, no open-source decoder) and achieve ~2× size reduction. We aim to match or beat that using fully open standards — JPEG XL for image data, zstd for metadata — while preserving every byte of the original NEF on decompression.

## Architecture

A NEF file is a TIFF container with several large binary chunks:

| Chunk | Contents | Typical size |
|---|---|---|
| **A** (raw strip) | Bayer sensor data, Nikon lossless compressed (Huffman-coded prediction residuals, 14-bit) | 20–25 MB |
| **B** (JpgFromRaw) | Camera-rendered full-res JPEG (~21 MP) | 1–3 MB |
| **C** (PreviewImage) | 640×424 JPEG thumbnail (in MakerNote PreviewIFD) | ~110 KB |
| **D** (Thumbnail) | 160×120 JPEG thumbnail (IFD0) | ~8 KB |
| **X** (skeleton) | Everything else: TIFF IFD tree, EXIF, MakerNote, GPS, etc. | ~50–200 KB |

### CNEF format (planned)

```
[magic "CNEF" + version]
[chunk table: count, per-chunk (type, original_offset, original_length, compressed_length)]
[compressed chunk A: Nikon decode → raw pixels → JPEG XL lossless]
[compressed chunk B: JPEG → JPEG XL JPEG recompression (bit-perfect)]
[compressed chunk C: JPEG → JPEG XL JPEG recompression]
[compressed chunk D: JPEG → JPEG XL JPEG recompression]
[compressed chunk X: zstd]
```

### Decompression (CNEF → NEF)

1. Decompress each chunk (JXL decode, zstd decode)
2. Re-encode chunk A back into Nikon lossless format (same Huffman tables, same prediction scheme → identical bitstream)
3. Reassemble the NEF: write skeleton X with TIFF offsets patched to point at the reconstructed chunk positions
4. Output is byte-for-byte identical to the original NEF

## Current State

### Done

- **TIFF/IFD parser** (`src/tiff.rs`): walks NEF TIFF structure including nested MakerNote embedded TIFF. Adapted from nefrite's `index.rs`.
- **NEF scanner** (`src/nef.rs`): locates all chunks (raw strip, JpgFromRaw, PreviewImage, Thumbnail) with their file offsets and dimensions. Also reads Nikon lossless compression metadata (Huffman table index, initial predictors, split row) from the MakerNote.
- **Nikon lossless codec** (`src/nikon_lossless.rs`): both decoder and encoder. Uses the 6 predefined Huffman table sets from rawspeed/rawloader. Handles the two-channel interleaved prediction scheme (even/odd Bayer columns), row-start predictor reset, and split-row table switching.
- **Roundtrip test** (`src/bin/roundtrip_test.rs`): reads a NEF, decodes the raw strip, re-encodes, asserts byte-for-byte equality. Tested on 3 legacy NEF files from Z50 II — all pass bit-perfect.
- **CNEF container format** (`src/cnef.rs`): binary container that splits the NEF into segments at chunk boundaries, compresses each independently, and reconstructs the original NEF byte-for-byte on decompression. Segments are ordered by file offset — decompression concatenates them without TIFF offset patching.
- **CLI** (`src/main.rs`): `compress`, `decompress`, and `info` commands.
- **Full NEF → CNEF → NEF roundtrip verified** on all 3 test files with `diff`.
- **JPEG XL integration** (`src/jxl.rs`): wraps libjxl (vendored via `jpegxl-rs`) for both raw pixel encoding and JPEG recompression.
  - **Raw strip**: Nikon decode → Bayer deinterleave (4 half-res channels) → JXL lossless. The deinterleave was the key insight: JXL compressing the raw mosaic barely beat Nikon's Huffman, but deinterleaving into spatially-coherent channels dropped the raw strip to 79–96% of original.
  - **JPEG thumbnails**: JXL JPEG recompression (71–91% of original, bit-perfect JPEG reconstruction).
  - **Skeleton metadata**: zstd (42–89% of original).
- **Per-segment compression stats** in CLI output.

#### Compression results (3 Z50 II test files, 14-bit lossless NEF):

| File | Original | CNEF | Ratio | Raw strip ratio |
|---|---|---|---|---|
| DSC_1250 | 25.5 MB | 22.7 MB | 88.9% | 90.1% |
| DSC_1304 | 28.5 MB | 26.8 MB | 94.4% | 96.1% |
| DSC_3711 | 22.0 MB | 17.4 MB | 79.1% | 79.2% |

#### What was tried and didn't help:
- **G-channel lifting** (store avg(G1,G2) + delta): −0.4% to +0.9%, essentially noise. JXL modular mode already decorrelates the G channels via MA trees.
- **RGB+alpha arrangement** (R,G1,B as color + G2 as alpha): +0.1% to +0.9% worse. The alpha channel gets inferior treatment (no RCT).
- **JXL effort 9**: ~0.5% better compression for 5× longer encode time.
- **14-bit vs 16-bit declared depth**: identical compression — JXL modular mode adapts to actual value range.

### Future

- **Batch mode**: compress/decompress entire directories
- **Verification mode**: `nef-compactor verify <file.cnef>` — decompress and diff against original without writing
- **12-bit NEF support**: current codec handles both 12-bit and 14-bit, but only tested on 14-bit Z50 II files
- **Lossy NEF support**: the codec supports lossy Huffman table indices and split-row, but lossy NEFs also apply a linearization curve + dithering — round-tripping through that is more complex
- **HE/HE★ NEF**: these use TicoRAW (HEVC-based), not Huffman lossless. Would need a different raw codec path, or fall back to byte-level zstd for the raw strip
- **Stretch: camera-compatible output**: make the CNEF look like a valid legacy NEF from the surface so Nikon cameras can preview it — would require careful TIFF structure mimicry

## Algorithm Reference

The Nikon lossless compression algorithm is well-documented across several open-source implementations:

- **rawspeed** (darktable): `NikonDecompressor.cpp` — cleanest C++ reference
- **rawloader**: `decoders/nef.rs` + `decoders/ljpeg/huffman.rs` — Rust reference
- **dcraw** (Dave Coffin): original reverse-engineering, single C file

The encoder in this project was written by studying these decoders and implementing the inverse. The algorithm is deterministic — same pixel values + same Huffman tables + same prediction scheme = identical bitstream. Verified empirically on real NEF files.
