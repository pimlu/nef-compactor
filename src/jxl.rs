use jpegxl_rs::decode::Data;
use jpegxl_rs::decoder_builder;
use jpegxl_rs::encoder_builder;

/// Encode Bayer mosaic pixels as lossless JPEG XL by deinterleaving
/// into 4 half-resolution channels (the 2x2 Bayer phases).
/// Width and height must be even.
pub fn encode_pixels(
    pixels: &[u16],
    width: u32,
    height: u32,
    bits_per_sample: u32,
    effort: i64,
) -> Result<Vec<u8>, String> {
    if width % 2 != 0 || height % 2 != 0 {
        return Err(format!("dimensions must be even for Bayer deinterleave: {width}x{height}"));
    }
    let half_w = width / 2;
    let half_h = height / 2;

    let interleaved = bayer_deinterleave(pixels, width as usize, height as usize);
    let jxl = unsafe { encode_4ch_ffi(&interleaved, half_w, half_h, bits_per_sample, effort) }?;
    Ok(jxl)
}

/// Decode lossless JPEG XL back to Bayer mosaic pixels by reinterleaving
/// the 4 channels.
pub fn decode_pixels(
    jxl_data: &[u8],
    full_width: u32,
    full_height: u32,
) -> Result<Vec<u16>, String> {
    let (interleaved, half_w, half_h) = unsafe { decode_4ch_ffi(jxl_data) }?;
    if half_w * 2 != full_width || half_h * 2 != full_height {
        return Err(format!(
            "JXL dimensions {}x{} don't match expected {}x{}",
            half_w * 2, half_h * 2, full_width, full_height,
        ));
    }
    Ok(bayer_reinterleave(&interleaved, full_width as usize, full_height as usize))
}

/// Losslessly recompress JPEG bytes into JPEG XL.
pub fn encode_jpeg(jpeg_data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = encoder_builder()
        .build()
        .map_err(|e| format!("jxl jpeg encoder init: {e}"))?;

    let result = encoder
        .encode_jpeg(jpeg_data)
        .map_err(|e| format!("jxl jpeg encode: {e}"))?;
    Ok(result.data)
}

/// Reconstruct the original JPEG bytes from a JXL file.
pub fn decode_jpeg(jxl_data: &[u8]) -> Result<Vec<u8>, String> {
    let decoder = decoder_builder()
        .build()
        .map_err(|e| format!("jxl decoder init: {e}"))?;

    let (_metadata, data) = decoder
        .reconstruct(jxl_data)
        .map_err(|e| format!("jxl jpeg reconstruct: {e}"))?;

    match data {
        Data::Jpeg(bytes) => Ok(bytes),
        Data::Pixels(_) => Err("JXL file doesn't contain JPEG reconstruction data".into()),
    }
}

// ─── Bayer deinterleave / reinterleave ───────────────────────────────────────

/// Deinterleave Bayer mosaic into 4-channel interleaved buffer.
/// ch0: (even_row, even_col), ch1: (even_row, odd_col),
/// ch2: (odd_row, even_col),  ch3: (odd_row, odd_col).
///
/// All 4 Bayer phases become separate channels at half resolution,
/// letting JXL's predictors see spatially coherent data and exploit
/// cross-channel correlations.
fn bayer_deinterleave(mosaic: &[u16], width: usize, height: usize) -> Vec<u16> {
    let half_w = width / 2;
    let half_h = height / 2;
    let mut out = vec![0u16; half_w * half_h * 4];

    for row in 0..half_h {
        for col in 0..half_w {
            let dst = (row * half_w + col) * 4;
            out[dst] = mosaic[(row * 2) * width + (col * 2)];
            out[dst + 1] = mosaic[(row * 2) * width + (col * 2 + 1)];
            out[dst + 2] = mosaic[(row * 2 + 1) * width + (col * 2)];
            out[dst + 3] = mosaic[(row * 2 + 1) * width + (col * 2 + 1)];
        }
    }
    out
}

/// Reinterleave 4-channel buffer back into Bayer mosaic.
fn bayer_reinterleave(interleaved: &[u16], width: usize, height: usize) -> Vec<u16> {
    let half_w = width / 2;
    let half_h = height / 2;
    let mut mosaic = vec![0u16; width * height];

    for row in 0..half_h {
        for col in 0..half_w {
            let src = (row * half_w + col) * 4;
            mosaic[(row * 2) * width + (col * 2)] = interleaved[src];
            mosaic[(row * 2) * width + (col * 2 + 1)] = interleaved[src + 1];
            mosaic[(row * 2 + 1) * width + (col * 2)] = interleaved[src + 2];
            mosaic[(row * 2 + 1) * width + (col * 2 + 1)] = interleaved[src + 3];
        }
    }
    mosaic
}

// ─── FFI 4-channel encoder ───────────────────────────────────────────────────

unsafe fn encode_4ch_ffi(
    pixels: &[u16],
    width: u32,
    height: u32,
    bits_per_sample: u32,
    effort: i64,
) -> Result<Vec<u8>, String> {
    use jpegxl_sys::common::types::*;
    use jpegxl_sys::encoder::encode::*;
    use jpegxl_sys::metadata::codestream_header::JxlExtraChannelType;
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::ptr;

    let enc = unsafe { JxlEncoderCreate(ptr::null()) };
    if enc.is_null() {
        return Err("JxlEncoderCreate failed".into());
    }
    let _guard = EncGuard(enc);

    let mut info = unsafe {
        let mut info = MaybeUninit::uninit();
        JxlEncoderInitBasicInfo(info.as_mut_ptr());
        info.assume_init()
    };
    info.xsize = width;
    info.ysize = height;
    info.bits_per_sample = bits_per_sample;
    info.exponent_bits_per_sample = 0;
    info.num_color_channels = 3;
    info.num_extra_channels = 1;
    info.alpha_bits = bits_per_sample;
    info.alpha_exponent_bits = 0;
    info.uses_original_profile = true.into();

    enc_check(unsafe { JxlEncoderSetBasicInfo(enc, &info) }, "SetBasicInfo")?;

    let mut ec_info = unsafe {
        let mut ec = MaybeUninit::uninit();
        JxlEncoderInitExtraChannelInfo(JxlExtraChannelType::Alpha, ec.as_mut_ptr());
        ec.assume_init()
    };
    ec_info.bits_per_sample = bits_per_sample;
    enc_check(
        unsafe { JxlEncoderSetExtraChannelInfo(enc, 0, &ec_info) },
        "SetExtraChannelInfo",
    )?;

    let color = unsafe {
        let mut c = MaybeUninit::uninit();
        JxlColorEncodingSetToSRGB(c.as_mut_ptr(), false);
        c.assume_init()
    };
    enc_check(
        unsafe { JxlEncoderSetColorEncoding(enc, &color) },
        "SetColorEncoding",
    )?;

    let opts = unsafe { JxlEncoderFrameSettingsCreate(enc, ptr::null()) };
    if opts.is_null() {
        return Err("JxlEncoderFrameSettingsCreate failed".into());
    }
    enc_check(
        unsafe { JxlEncoderSetFrameLossless(opts, true.into()) },
        "SetFrameLossless",
    )?;
    enc_check(
        unsafe {
            JxlEncoderFrameSettingsSetOption(opts, JxlEncoderFrameSettingId::Effort, effort)
        },
        "SetEffort",
    )?;

    let bit_depth = JxlBitDepth {
        r#type: JxlBitDepthType::FromCodestream,
        bits_per_sample,
        exponent_bits_per_sample: 0,
    };
    enc_check(
        unsafe { JxlEncoderSetFrameBitDepth(opts, &bit_depth) },
        "SetFrameBitDepth",
    )?;

    // 4-channel interleaved: 3 color + 1 alpha
    let pixel_format = JxlPixelFormat {
        num_channels: 4,
        data_type: JxlDataType::Uint16,
        endianness: JxlEndianness::Native,
        align: 0,
    };
    enc_check(
        unsafe {
            JxlEncoderAddImageFrame(
                opts,
                &pixel_format,
                pixels.as_ptr() as *const c_void,
                pixels.len() * 2,
            )
        },
        "AddImageFrame",
    )?;

    unsafe { JxlEncoderCloseInput(enc) };

    drain_encoder(enc)
}

fn drain_encoder(enc: *mut jpegxl_sys::encoder::encode::JxlEncoder) -> Result<Vec<u8>, String> {
    use jpegxl_sys::encoder::encode::*;

    let mut output = vec![0u8; 1024 * 1024];
    let mut offset = 0usize;
    loop {
        let mut next_out = unsafe { output.as_mut_ptr().add(offset) };
        let mut avail_out = output.len() - offset;

        let status =
            unsafe { JxlEncoderProcessOutput(enc, &mut next_out, &mut avail_out) };
        offset = output.len() - avail_out;

        match status {
            JxlEncoderStatus::Success => {
                output.truncate(offset);
                return Ok(output);
            }
            JxlEncoderStatus::NeedMoreOutput => {
                output.resize(output.len() * 2, 0);
            }
            JxlEncoderStatus::Error => {
                let err = unsafe { JxlEncoderGetError(enc) };
                return Err(format!("JxlEncoderProcessOutput: {err:?}"));
            }
        }
    }
}

// ─── FFI 4-channel decoder ───────────────────────────────────────────────────

unsafe fn decode_4ch_ffi(jxl_data: &[u8]) -> Result<(Vec<u16>, u32, u32), String> {
    use jpegxl_sys::common::types::*;
    use jpegxl_sys::decode::*;
    use jpegxl_sys::metadata::codestream_header::JxlBasicInfo;
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::ptr;

    let dec = unsafe { JxlDecoderCreate(ptr::null()) };
    if dec.is_null() {
        return Err("JxlDecoderCreate failed".into());
    }
    struct DecGuard(*mut JxlDecoder);
    impl Drop for DecGuard {
        fn drop(&mut self) {
            unsafe { JxlDecoderDestroy(self.0) };
        }
    }
    let _guard = DecGuard(dec);

    dec_check(
        unsafe {
            JxlDecoderSubscribeEvents(
                dec,
                (JxlDecoderStatus::BasicInfo as i32) | (JxlDecoderStatus::FullImage as i32),
            )
        },
        "SubscribeEvents",
    )?;

    unsafe { JxlDecoderSetInput(dec, jxl_data.as_ptr(), jxl_data.len()) };
    unsafe { JxlDecoderCloseInput(dec) };

    let mut width = 0u32;
    let mut height = 0u32;
    let mut pixels: Vec<u16> = Vec::new();

    loop {
        let status = unsafe { JxlDecoderProcessInput(dec) };
        match status {
            JxlDecoderStatus::BasicInfo => {
                let mut info = unsafe { MaybeUninit::<JxlBasicInfo>::zeroed().assume_init() };
                dec_check(
                    unsafe { JxlDecoderGetBasicInfo(dec, &mut info) },
                    "GetBasicInfo",
                )?;
                width = info.xsize;
                height = info.ysize;
            }
            JxlDecoderStatus::NeedImageOutBuffer => {
                // 4 channels interleaved
                let pixel_format = JxlPixelFormat {
                    num_channels: 4,
                    data_type: JxlDataType::Uint16,
                    endianness: JxlEndianness::Native,
                    align: 0,
                };
                let n_pixels = (width as usize) * (height as usize) * 4;
                let buf_size = n_pixels * 2;
                pixels = vec![0u16; n_pixels];
                dec_check(
                    unsafe {
                        JxlDecoderSetImageOutBuffer(
                            dec,
                            &pixel_format,
                            pixels.as_mut_ptr() as *mut c_void,
                            buf_size,
                        )
                    },
                    "SetImageOutBuffer",
                )?;

                let bit_depth = JxlBitDepth {
                    r#type: JxlBitDepthType::FromCodestream,
                    bits_per_sample: 0,
                    exponent_bits_per_sample: 0,
                };
                dec_check(
                    unsafe { JxlDecoderSetImageOutBitDepth(dec, &bit_depth) },
                    "SetImageOutBitDepth",
                )?;
            }
            JxlDecoderStatus::FullImage => {}
            JxlDecoderStatus::Success => {
                return Ok((pixels, width, height));
            }
            JxlDecoderStatus::NeedMoreInput => {
                return Err("JXL decoder needs more input (truncated?)".into());
            }
            other => {
                return Err(format!("JXL decoder unexpected status: {other:?}"));
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

struct EncGuard(*mut jpegxl_sys::encoder::encode::JxlEncoder);
impl Drop for EncGuard {
    fn drop(&mut self) {
        unsafe { jpegxl_sys::encoder::encode::JxlEncoderDestroy(self.0) };
    }
}

fn enc_check(
    status: jpegxl_sys::encoder::encode::JxlEncoderStatus,
    op: &str,
) -> Result<(), String> {
    if status == jpegxl_sys::encoder::encode::JxlEncoderStatus::Success {
        Ok(())
    } else {
        Err(format!("JxlEncoder {op}: {status:?}"))
    }
}

fn dec_check(
    status: jpegxl_sys::decode::JxlDecoderStatus,
    op: &str,
) -> Result<(), String> {
    if status == jpegxl_sys::decode::JxlDecoderStatus::Success {
        Ok(())
    } else {
        Err(format!("JxlDecoder {op}: {status:?}"))
    }
}
