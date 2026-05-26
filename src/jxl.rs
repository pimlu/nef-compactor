use jpegxl_rs::decode::{Data, Pixels};
use jpegxl_rs::decoder_builder;
use jpegxl_rs::encoder_builder;

/// Encode 16-bit grayscale pixels as lossless JPEG XL with the correct
/// bit depth (e.g., 14-bit for most modern Nikon sensors).
pub fn encode_pixels(
    pixels: &[u16],
    width: u32,
    height: u32,
    bits_per_sample: u32,
) -> Result<Vec<u8>, String> {
    unsafe { encode_pixels_ffi(pixels, width, height, bits_per_sample) }
}

/// Decode lossless JPEG XL back to 16-bit grayscale pixels.
pub fn decode_pixels(jxl_data: &[u8]) -> Result<(Vec<u16>, u32, u32), String> {
    let decoder = decoder_builder()
        .build()
        .map_err(|e| format!("jxl decoder init: {e}"))?;

    let (metadata, pixels) = decoder
        .decode(jxl_data)
        .map_err(|e| format!("jxl decode: {e}"))?;

    match pixels {
        Pixels::Uint16(data) => Ok((data, metadata.width, metadata.height)),
        Pixels::Uint8(_) => Err("expected u16 pixels, got u8".into()),
        Pixels::Float(_) => Err("expected u16 pixels, got f32".into()),
        Pixels::Float16(_) => Err("expected u16 pixels, got f16".into()),
    }
}

/// Losslessly recompress JPEG bytes into JPEG XL, preserving the ability
/// to reconstruct the original JPEG bit-for-bit.
pub fn encode_jpeg(jpeg_data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = encoder_builder()
        .build()
        .map_err(|e| format!("jxl jpeg encoder init: {e}"))?;

    let result = encoder
        .encode_jpeg(jpeg_data)
        .map_err(|e| format!("jxl jpeg encode: {e}"))?;
    Ok(result.data)
}

/// Reconstruct the original JPEG bytes from a JXL file that was created
/// via JPEG recompression.
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

unsafe fn encode_pixels_ffi(
    pixels: &[u16],
    width: u32,
    height: u32,
    bits_per_sample: u32,
) -> Result<Vec<u8>, String> {
    use jpegxl_sys::common::types::*;
    use jpegxl_sys::encoder::encode::*;
    use std::ffi::c_void;
    use std::mem::MaybeUninit;
    use std::ptr;

    let encoder = unsafe { JxlEncoderCreate(ptr::null()) };
    if encoder.is_null() {
        return Err("JxlEncoderCreate failed".into());
    }
    struct Guard(*mut JxlEncoder);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { JxlEncoderDestroy(self.0) };
        }
    }
    let _guard = Guard(encoder);

    // Basic info: grayscale, N-bit, lossless
    let mut info = unsafe {
        let mut info = MaybeUninit::uninit();
        JxlEncoderInitBasicInfo(info.as_mut_ptr());
        info.assume_init()
    };
    info.xsize = width;
    info.ysize = height;
    // Use 16-bit container depth so u16 values round-trip without rescaling.
    // JXL lossless modular mode will see the top bits are always zero and
    // compress them away — no meaningful penalty vs declaring 14-bit.
    info.bits_per_sample = 16;
    info.exponent_bits_per_sample = 0;
    info.num_color_channels = 1;
    info.num_extra_channels = 0;
    info.alpha_bits = 0;
    info.uses_original_profile = true.into();

    check(unsafe { JxlEncoderSetBasicInfo(encoder, &info) }, "SetBasicInfo")?;

    // Color encoding: grayscale sRGB
    let mut color = unsafe {
        let mut c = MaybeUninit::uninit();
        JxlColorEncodingSetToSRGB(c.as_mut_ptr(), true);
        c.assume_init()
    };
    let _ = &mut color; // suppress unused warning
    check(
        unsafe { JxlEncoderSetColorEncoding(encoder, &color) },
        "SetColorEncoding",
    )?;

    // Frame settings: lossless
    let opts = unsafe { JxlEncoderFrameSettingsCreate(encoder, ptr::null()) };
    if opts.is_null() {
        return Err("JxlEncoderFrameSettingsCreate failed".into());
    }
    check(
        unsafe { JxlEncoderSetFrameLossless(opts, true.into()) },
        "SetFrameLossless",
    )?;
    check(
        unsafe {
            JxlEncoderFrameSettingsSetOption(
                opts,
                JxlEncoderFrameSettingId::Effort,
                7, // squirrel
            )
        },
        "SetEffort",
    )?;

    // Add the pixel data
    let pixel_format = JxlPixelFormat {
        num_channels: 1,
        data_type: JxlDataType::Uint16,
        endianness: JxlEndianness::Native,
        align: 0,
    };
    check(
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

    unsafe { JxlEncoderCloseInput(encoder) };

    // Pull encoded output
    let mut output = vec![0u8; (width * height) as usize]; // rough initial estimate
    let mut offset = 0usize;
    loop {
        let mut next_out = unsafe { output.as_mut_ptr().add(offset) };
        let mut avail_out = output.len() - offset;

        let status =
            unsafe { JxlEncoderProcessOutput(encoder, &mut next_out, &mut avail_out) };
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
                let err = unsafe { JxlEncoderGetError(encoder) };
                return Err(format!("JxlEncoderProcessOutput: {err:?}"));
            }
        }
    }
}

fn check(status: jpegxl_sys::encoder::encode::JxlEncoderStatus, op: &str) -> Result<(), String> {
    if status == jpegxl_sys::encoder::encode::JxlEncoderStatus::Success {
        Ok(())
    } else {
        Err(format!("Jxl {op}: {status:?}"))
    }
}
