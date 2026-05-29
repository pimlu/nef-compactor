//! Sample layouts for "uncompressed" Nikon NEFs.
//!
//! Two layouts occur, distinguished purely by strip size (see [`Layout::detect`])
//! so both compress and decompress infer the same one without storing a flag:
//!  - `Packed`: samples are `bps` bits, LSB-first, rows padded to `stride`
//!    bytes (12-bit on all bodies; 14-bit on e.g. the Z-series).
//!  - `Unpacked16`: each sample is a little-endian 16-bit word, rows padded to
//!    `stride` bytes (14-bit on e.g. the D850).
//!
//! [`pack`] is the exact inverse of [`unpack`]; row padding (and any unused
//! high bits of a packed row's last byte) is assumed zero. Anything that
//! doesn't repack identically is skipped by the caller, never corrupted.

#[derive(Clone, Copy)]
pub enum Layout {
    Packed { bps: u32, stride: usize },
    Unpacked16 { stride: usize },
}

impl Layout {
    /// Infer an uncompressed strip's layout from its byte length. Returns `None`
    /// if the strip doesn't tile into equal rows large enough for `width`
    /// samples.
    pub fn detect(length: usize, width: usize, height: usize, bps: u32) -> Option<Self> {
        if height == 0 || length % height != 0 {
            return None;
        }
        let stride = length / height;
        if length >= width * height * 2 {
            (stride >= width * 2).then_some(Layout::Unpacked16 { stride })
        } else {
            (stride >= (width * bps as usize).div_ceil(8)).then_some(Layout::Packed { bps, stride })
        }
    }

    /// Bit depth to hand JPEG XL. Unpacked words are treated as full 16-bit so
    /// every value round-trips regardless of unused high bits.
    pub fn jxl_bps(self) -> u32 {
        match self {
            Layout::Packed { bps, .. } => bps,
            Layout::Unpacked16 { .. } => 16,
        }
    }
}

/// Unpack `width*height` samples into a Bayer mosaic buffer.
pub fn unpack(data: &[u8], width: usize, height: usize, layout: Layout) -> Vec<u16> {
    let mut out = vec![0u16; width * height];
    match layout {
        Layout::Packed { bps, stride } => {
            let mask = (1u32 << bps) - 1;
            for row in 0..height {
                let rb = &data[row * stride..(row + 1) * stride];
                let (mut acc, mut nbits, mut bi) = (0u32, 0u32, 0usize);
                for col in 0..width {
                    while nbits < bps {
                        acc |= (rb[bi] as u32) << nbits;
                        bi += 1;
                        nbits += 8;
                    }
                    out[row * width + col] = (acc & mask) as u16;
                    acc >>= bps;
                    nbits -= bps;
                }
            }
        }
        Layout::Unpacked16 { stride } => {
            for row in 0..height {
                let rb = &data[row * stride..];
                for col in 0..width {
                    out[row * width + col] = u16::from_le_bytes([rb[2 * col], rb[2 * col + 1]]);
                }
            }
        }
    }
    out
}

/// Repack samples into the byte layout (the exact inverse of [`unpack`]);
/// padding past the samples is left zero.
pub fn pack(pixels: &[u16], width: usize, height: usize, layout: Layout) -> Vec<u8> {
    let stride = match layout {
        Layout::Packed { stride, .. } | Layout::Unpacked16 { stride } => stride,
    };
    let mut out = vec![0u8; height * stride];
    for row in 0..height {
        let rb = &mut out[row * stride..(row + 1) * stride];
        match layout {
            Layout::Packed { bps, .. } => {
                let (mut acc, mut nbits, mut bi) = (0u32, 0u32, 0usize);
                for col in 0..width {
                    acc |= (pixels[row * width + col] as u32) << nbits;
                    nbits += bps;
                    while nbits >= 8 {
                        rb[bi] = acc as u8;
                        bi += 1;
                        acc >>= 8;
                        nbits -= 8;
                    }
                }
                if nbits > 0 {
                    rb[bi] = acc as u8;
                }
            }
            Layout::Unpacked16 { .. } => {
                for col in 0..width {
                    let [lo, hi] = pixels[row * width + col].to_le_bytes();
                    rb[2 * col] = lo;
                    rb[2 * col + 1] = hi;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrip() {
        let (w, h) = (16usize, 4usize);
        let layouts = [
            Layout::Packed { bps: 14, stride: (16 * 14) / 8 + 8 }, // 14-bit + pad
            Layout::Packed { bps: 12, stride: (16 * 12) / 8 },     // 12-bit, no pad
            Layout::Unpacked16 { stride: 16 * 2 },                 // 16-bit words
        ];
        for layout in layouts {
            let bps = layout.jxl_bps().min(14);
            let pixels: Vec<u16> = (0..(w * h) as u32)
                .map(|i| ((i * 37 + 11) & ((1 << bps) - 1)) as u16)
                .collect();
            let bytes = pack(&pixels, w, h, layout);
            assert_eq!(unpack(&bytes, w, h, layout), pixels);
            assert_eq!(pack(&unpack(&bytes, w, h, layout), w, h, layout), bytes);
        }
    }
}
