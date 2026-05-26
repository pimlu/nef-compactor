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
- **NEF scanner** (`src/nef.rs`): locates all chunks (raw strip, JpgFromRaw, PreviewImage, Thumbnail) with their file offsets and dimensions.
- **Nikon lossless codec** (`src/nikon_lossless.rs`): both decoder and encoder. Uses the 6 predefined Huffman table sets from rawspeed/rawloader. Handles the two-channel interleaved prediction scheme (even/odd Bayer columns), row-start predictor reset, and split-row table switching.
- **Roundtrip test** (`src/bin/roundtrip_test.rs`): reads a NEF, decodes the raw strip, re-encodes, asserts byte-for-byte equality. Tested on 3 legacy NEF files from Z50 II — all pass bit-perfect.

### Next: CNEF container format

- Define the binary container format (magic, chunk table, compressed payloads)
- Implement CNEF writer (NEF → CNEF compression)
- Implement CNEF reader (CNEF → NEF decompression)

### Next: JPEG XL integration

- Add `jxl-oxide` or `libjxl` dependency for JPEG XL encoding/decoding
- Raw strip: decode Nikon lossless → 14-bit Bayer pixels → JXL lossless encode
- JPEG thumbnails: JXL JPEG recompression (lossless transcode, ~20% savings, original JPEG reconstructible bit-for-bit)

### Next: NEF reconstruction

- Reassemble the full NEF file from decompressed chunks
- Patch TIFF IFD offset fields so all pointers are valid in the output
- Byte-for-byte verification against original NEF

### Next: zstd for metadata

- Compress the TIFF skeleton (chunk X) with zstd
- Expected savings small in absolute terms but free compression

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
