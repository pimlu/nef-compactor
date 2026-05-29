/// Nikon lossless compressed NEF codec.
///
/// The format is a JPEG-style Huffman coding of prediction residuals over
/// the raw Bayer image. Two interleaved prediction channels (even/odd
/// columns) with left-neighbor prediction, reset-to-up at each row start.
///
/// Algorithm reference: rawspeed NikonDecompressor.cpp, rawloader nef.rs

// Predefined Huffman table sets. Index 0-2: 12-bit (lossy, lossy-after-split,
// lossless). Index 3-5: 14-bit (same order). Each has [code_lengths; symbols].
//
// A symbol's low nibble is the diff magnitude category (number of payload
// bits); the high nibble is a left-shift used only by the lossy "after split"
// tables (1, 4) to drop low-order bits — i.e. the lossy quantization. The
// lossless tables (2, 5) and no-split lossy tables (0, 3) always have shift 0.
const NIKON_TREE: [[[u8; 16]; 2]; 6] = [
    [
        // 12-bit lossy
        [0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0],
        [5, 4, 3, 6, 2, 7, 1, 0, 8, 9, 11, 10, 12, 0, 0, 0],
    ],
    [
        // 12-bit lossy after split
        [0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0],
        [0x39, 0x5a, 0x38, 0x27, 0x16, 5, 4, 3, 2, 1, 0, 11, 12, 12, 0, 0],
    ],
    [
        // 12-bit lossless
        [0, 1, 4, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        [5, 4, 6, 3, 7, 2, 8, 1, 9, 0, 10, 11, 12, 0, 0, 0],
    ],
    [
        // 14-bit lossy
        [0, 1, 4, 3, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0],
        [5, 6, 4, 7, 8, 3, 9, 2, 1, 0, 10, 11, 12, 13, 14, 0],
    ],
    [
        // 14-bit lossy after split
        [0, 1, 5, 1, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0],
        [8, 0x5c, 0x4b, 0x3a, 0x29, 7, 6, 5, 4, 3, 2, 1, 0, 13, 14, 0],
    ],
    [
        // 14-bit lossless
        [0, 1, 4, 2, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0],
        [7, 6, 8, 5, 9, 4, 10, 3, 11, 12, 2, 0, 1, 13, 14, 0],
    ],
];

// ─── Huffman table ───────────────────────────────────────────────────────────

struct HuffEntry {
    symbol: u8,
    code: u16,
    code_len: u8,
}

struct HuffTable {
    entries: Vec<HuffEntry>,
    decode_table: Vec<(u8, u8)>, // (code_len, symbol) indexed by prefix bits
    max_bits: u8,
}

impl HuffTable {
    fn build(table_index: usize) -> Self {
        let code_lengths = &NIKON_TREE[table_index][0];
        let symbols = &NIKON_TREE[table_index][1];

        let mut entries = Vec::new();
        let mut code: u16 = 0;
        let mut sym_idx = 0;

        let mut max_bits: u8 = 0;
        for bit_len in 1u8..=16 {
            let count = code_lengths[(bit_len - 1) as usize];
            for _ in 0..count {
                entries.push(HuffEntry {
                    symbol: symbols[sym_idx],
                    code,
                    code_len: bit_len,
                });
                sym_idx += 1;
                code += 1;
                max_bits = bit_len;
            }
            code <<= 1;
        }

        let table_size = 1usize << max_bits;
        let mut decode_table = vec![(0u8, 0u8); table_size];
        for entry in &entries {
            let prefix = (entry.code as usize) << (max_bits - entry.code_len);
            let fill_count = 1usize << (max_bits - entry.code_len);
            for j in 0..fill_count {
                decode_table[prefix | j] = (entry.code_len, entry.symbol);
            }
        }

        HuffTable {
            entries,
            decode_table,
            max_bits,
        }
    }

    /// Find the entry whose magnitude category (low nibble) matches `len`.
    /// For the shifted "after split" tables the symbol byte also carries a
    /// shift in its high nibble, so we can't match on the raw symbol value.
    fn find_entry_for_len(&self, len: u8) -> Option<&HuffEntry> {
        self.entries.iter().find(|e| (e.symbol & 0x0F) == len)
    }
}

// ─── Bitstream reader ────────────────────────────────────────────────────────

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_buf: u64,
    bits_left: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        let mut r = BitReader {
            data,
            byte_pos: 0,
            bit_buf: 0,
            bits_left: 0,
        };
        r.fill();
        r
    }

    fn fill(&mut self) {
        while self.bits_left <= 56 && self.byte_pos < self.data.len() {
            self.bit_buf |= (self.data[self.byte_pos] as u64) << (56 - self.bits_left);
            self.byte_pos += 1;
            self.bits_left += 8;
        }
    }

    fn peek_bits(&mut self, n: u32) -> u32 {
        self.fill();
        (self.bit_buf >> (64 - n)) as u32
    }

    fn consume_bits(&mut self, n: u32) {
        self.bit_buf <<= n;
        // Saturate rather than wrap: a code at the very end of the stream can
        // ask for more bits than remain (the decoder is reading the final
        // zero-padding). Clamping keeps `total_bits_consumed` sane.
        self.bits_left = self.bits_left.saturating_sub(n);
    }

    fn get_bits(&mut self, n: u32) -> u32 {
        let val = self.peek_bits(n);
        self.consume_bits(n);
        val
    }

    fn total_bits_consumed(&self) -> usize {
        (self.byte_pos * 8).saturating_sub(self.bits_left as usize)
    }
}

// ─── Bitstream writer ────────────────────────────────────────────────────────

struct BitWriter {
    data: Vec<u8>,
    bit_buf: u64,
    bits_pending: u32,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            data: Vec::new(),
            bit_buf: 0,
            bits_pending: 0,
        }
    }

    fn write_bits(&mut self, value: u32, num_bits: u8) {
        self.bit_buf = (self.bit_buf << num_bits) | (value as u64);
        self.bits_pending += num_bits as u32;
        while self.bits_pending >= 8 {
            self.bits_pending -= 8;
            self.data.push((self.bit_buf >> self.bits_pending) as u8);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits_pending > 0 {
            let pad = 8 - self.bits_pending;
            let pad_mask = (1u64 << pad) - 1;
            self.data.push(((self.bit_buf << pad) | pad_mask) as u8);
        }
        self.data
    }
}

// ─── Decode ──────────────────────────────────────────────────────────────────

fn huff_decode_diff(bits: &mut BitReader, ht: &HuffTable) -> Result<i32, String> {
    let code = bits.peek_bits(ht.max_bits as u32);
    let (code_len, symbol) = ht.decode_table[code as usize];
    if code_len == 0 {
        return Err("Huffman: invalid code".into());
    }
    bits.consume_bits(code_len as u32);

    let len = (symbol & 0x0F) as u32;
    let shl = (symbol >> 4) as u32;
    if len == 0 {
        return Ok(0);
    }

    // Read `len - shl` payload bits, then reinsert the `shl` forced-zero
    // low-order bits. For the lossless tables shl == 0 and this is the
    // ordinary JPEG magnitude-category decode.
    let extra = bits.get_bits(len - shl);
    let diff_raw = (((extra << 1) + 1) << shl) >> 1;
    let mut diff = diff_raw as i32;
    if (diff_raw & (1 << (len - 1))) == 0 {
        diff -= (1i32 << len) - if shl == 0 { 1 } else { 0 };
    }
    Ok(diff)
}

pub struct DecodeResult {
    pub pixels: Vec<u16>,
    pub bits_consumed: usize,
}

/// Decode a Nikon lossless compressed raw strip to pre-curve pixel values.
/// Returns the pixel buffer and the number of bits consumed from the input,
/// so callers can identify any trailing data beyond the encoded pixels.
pub fn decode(
    compressed: &[u8],
    width: usize,
    height: usize,
    _bits_per_sample: u32,
    huff_select: usize,
    initial_predictors: [[i32; 2]; 2],
    split_row: usize,
) -> Result<DecodeResult, String> {
    if width == 0 || height == 0 || width % 2 != 0 {
        return Err(format!("invalid dimensions {width}x{height}"));
    }

    let mut bits = BitReader::new(compressed);
    let mut ht = HuffTable::build(huff_select);

    let mut p_up = initial_predictors;
    let mut out = vec![0u16; width * height];

    for row in 0..height {
        if split_row > 0 && row == split_row {
            ht = HuffTable::build(huff_select + 1);
        }

        let mut pred = p_up[row & 1];
        for col in 0..width {
            // Accumulate with wrapping: only the low 16 bits are stored, and a
            // corrupt stream shouldn't be able to overflow-panic a debug build.
            pred[col & 1] = pred[col & 1].wrapping_add(huff_decode_diff(&mut bits, &ht)?);
            if col < 2 {
                p_up[row & 1][col & 1] = pred[col & 1];
            }
            // Store the predictor's low 16 bits rather than clamping to the
            // sample range. The reference decoders keep the predictor chain
            // unclamped (lossy frames can run it out of range); clamping here
            // would make the re-encode diverge. Diffs are always within ±2^15,
            // so they're recovered exactly from consecutive values mod 2^16.
            out[row * width + col] = pred[col & 1] as u16;
        }
    }

    Ok(DecodeResult {
        pixels: out,
        bits_consumed: bits.total_bits_consumed(),
    })
}

// ─── Encode ──────────────────────────────────────────────────────────────────

fn encode_diff(writer: &mut BitWriter, ht: &HuffTable, diff: i32) -> Result<(), String> {
    if diff == 0 {
        let entry = ht.find_entry_for_len(0).ok_or("Huffman: table has no zero symbol")?;
        writer.write_bits(entry.code as u32, entry.code_len);
        return Ok(());
    }

    let abs_diff = diff.unsigned_abs();
    let len = 32 - abs_diff.leading_zeros(); // magnitude category
    let entry = ht.find_entry_for_len(len as u8).ok_or_else(|| {
        format!("Huffman: diff {diff} needs {len}-bit category absent from this table")
    })?;
    let shl = (entry.symbol >> 4) as u32;

    // Invert huff_decode_diff: undo the sign extension to recover diff_raw,
    // then strip the `shl` forced low bits to recover the payload. For shl == 0
    // this is the ordinary JPEG magnitude-category encode (extra == diff_raw).
    let diff_raw = if diff < 0 {
        (diff + (1i32 << len) - if shl == 0 { 1 } else { 0 }) as u32
    } else {
        diff as u32
    };
    let extra = if shl == 0 {
        diff_raw
    } else {
        ((diff_raw >> (shl - 1)) - 1) >> 1
    };

    writer.write_bits(entry.code as u32, entry.code_len);
    writer.write_bits(extra, (len - shl) as u8);
    Ok(())
}

/// Encode pixel values back into a Nikon lossless compressed bitstream. The
/// inverse of `decode`. Pixel values are the stored values `decode` produces
/// (the predictor's low 16 bits); diffs are taken mod 2^16 and recovered
/// exactly because they're always within ±2^15.
pub fn encode(
    pixels: &[u16],
    width: usize,
    height: usize,
    _bits_per_sample: u32,
    huff_select: usize,
    initial_predictors: [[i32; 2]; 2],
    split_row: usize,
) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 || width % 2 != 0 {
        return Err(format!("invalid dimensions {width}x{height}"));
    }
    if pixels.len() != width * height {
        return Err(format!(
            "pixel count mismatch: {} vs {}x{}",
            pixels.len(),
            width,
            height
        ));
    }

    let mut writer = BitWriter::new();
    let mut ht = HuffTable::build(huff_select);

    // Predictors as u16 (the low 16 bits decode stores); diffs are mod 2^16.
    let mut p_up = initial_predictors.map(|row| [row[0] as u16, row[1] as u16]);

    for row in 0..height {
        if split_row > 0 && row == split_row {
            ht = HuffTable::build(huff_select + 1);
        }

        let mut pred = p_up[row & 1];
        for col in 0..width {
            let pixel = pixels[row * width + col];
            let diff = pixel.wrapping_sub(pred[col & 1]) as i16 as i32;
            encode_diff(&mut writer, &ht, diff)?;
            pred[col & 1] = pixel;
            if col < 2 {
                p_up[row & 1][col & 1] = pixel;
            }
        }
    }

    Ok(writer.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hufftable_roundtrip() {
        // Verify that encoding then decoding each possible diff value
        // produces the original value, for each lossless table.
        for table_idx in [2, 5] {
            // 12-bit lossless, 14-bit lossless
            let ht = HuffTable::build(table_idx);
            let bps = if table_idx < 3 { 12 } else { 14 };
            let max_val = (1i32 << bps) - 1;

            for diff in -max_val..=max_val {
                let mut writer = BitWriter::new();
                encode_diff(&mut writer, &ht, diff).unwrap();
                let encoded = writer.finish();

                let mut reader = BitReader::new(&encoded);
                let decoded = huff_decode_diff(&mut reader, &ht).unwrap();
                assert_eq!(
                    diff, decoded,
                    "table {table_idx}: diff {diff} roundtrip failed"
                );
            }
        }
    }

    #[test]
    fn shifted_tables_roundtrip() {
        // The lossy "after split" tables (1, 4) carry a shift in the symbol's
        // high nibble. Enumerate every representable diff (by decoding every
        // payload value for every symbol) and confirm encode∘decode is identity.
        for table_idx in [1, 4] {
            let ht = HuffTable::build(table_idx);

            let mut diffs = std::collections::BTreeSet::new();
            for entry in &ht.entries {
                let len = (entry.symbol & 0x0F) as u32;
                let shl = (entry.symbol >> 4) as u32;
                if len == 0 {
                    diffs.insert(0i32);
                    continue;
                }
                for e in 0..(1u32 << (len - shl)) {
                    let diff_raw = (((e << 1) + 1) << shl) >> 1;
                    let mut diff = diff_raw as i32;
                    if (diff_raw & (1 << (len - 1))) == 0 {
                        diff -= (1i32 << len) - if shl == 0 { 1 } else { 0 };
                    }
                    diffs.insert(diff);
                }
            }

            for &diff in &diffs {
                let mut writer = BitWriter::new();
                encode_diff(&mut writer, &ht, diff).unwrap();
                let encoded = writer.finish();
                let mut reader = BitReader::new(&encoded);
                let decoded = huff_decode_diff(&mut reader, &ht).unwrap();
                assert_eq!(diff, decoded, "table {table_idx}: diff {diff} roundtrip failed");
            }
        }
    }

    #[test]
    fn encode_decode_small_image() {
        let width = 4;
        let height = 2;
        let bps = 14;
        let huff_select = 5; // 14-bit lossless
        let initial_pred = [[1000, 1000], [1000, 1000]];

        let pixels: Vec<u16> = vec![1000, 1001, 1002, 1003, 1004, 1005, 1006, 1007];

        let encoded = encode(&pixels, width, height, bps, huff_select, initial_pred, 0).unwrap();
        let result = decode(&encoded, width, height, bps, huff_select, initial_pred, 0).unwrap();
        let decoded = result.pixels;

        assert_eq!(pixels, decoded);
    }
}
