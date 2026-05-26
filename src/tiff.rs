use std::io::{Read, Seek, SeekFrom};

#[derive(Debug, Clone, Copy)]
pub struct TiffView {
    pub little_endian: bool,
    pub base: usize,
}

#[derive(Clone, Debug)]
pub struct RawEntry {
    pub tag: u16,
    pub field_type: u16,
    pub count: u32,
    pub value_bytes: [u8; 4],
}

pub const TYPE_BYTE: u16 = 1;
pub const TYPE_SHORT: u16 = 3;
pub const TYPE_LONG: u16 = 4;
pub const TYPE_UNDEFINED: u16 = 7;

impl RawEntry {
    pub fn byte_size(&self) -> usize {
        let shift = match self.field_type {
            TYPE_SHORT => 1,
            TYPE_LONG => 2,
            5 => 3, // RATIONAL
            _ => 0, // BYTE, ASCII, UNDEFINED
        };
        (self.count as usize) << shift
    }
}

pub const TAG_IMAGE_WIDTH: u16 = 0x0100;
pub const TAG_IMAGE_LENGTH: u16 = 0x0101;
pub const TAG_BITS_PER_SAMPLE: u16 = 0x0102;
pub const TAG_COMPRESSION: u16 = 0x0103;
pub const TAG_STRIP_OFFSETS: u16 = 0x0111;
pub const TAG_ORIENTATION: u16 = 0x0112;
pub const TAG_STRIP_BYTE_COUNTS: u16 = 0x0117;
pub const TAG_JPEG_INTERCHANGE_FORMAT: u16 = 0x0201;
pub const TAG_JPEG_INTERCHANGE_FORMAT_LENGTH: u16 = 0x0202;
pub const TAG_SUB_IFDS: u16 = 0x014A;
pub const TAG_EXIF_IFD: u16 = 0x8769;
pub const TAG_MAKERNOTE: u16 = 0x927C;
pub const TAG_NIKON_PREVIEW_IFD: u16 = 0x0011;

pub const COMPRESSION_NIKON_LOSSLESS: u32 = 34713;
pub const COMPRESSION_JPEG_OLD_STYLE: u32 = 6;
pub const COMPRESSION_JPEG: u32 = 7;

impl TiffView {
    pub fn parse_header<R: Read + Seek>(
        reader: &mut R,
        header_start: usize,
    ) -> Result<(Self, u32), String> {
        reader
            .seek(SeekFrom::Start(header_start as u64))
            .map_err(|e| format!("TIFF: seek to header at {header_start}: {e}"))?;
        let mut buf = [0u8; 8];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("TIFF: header read at {header_start}: {e}"))?;
        let little_endian = match &buf[..2] {
            b"II" => true,
            b"MM" => false,
            other => return Err(format!("TIFF: bad endian mark {other:02X?}")),
        };
        let view = TiffView {
            little_endian,
            base: header_start,
        };
        let magic = view.decode_u16(&buf[2..4]);
        if magic != 0x002A {
            return Err(format!("TIFF: bad magic 0x{magic:04X}"));
        }
        let first_ifd = view.decode_u32(&buf[4..8]);
        Ok((view, first_ifd))
    }

    pub fn decode_u16(&self, b: &[u8]) -> u16 {
        if self.little_endian {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    }

    pub fn decode_u32(&self, b: &[u8]) -> u32 {
        if self.little_endian {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    }

    pub fn encode_u16(&self, val: u16) -> [u8; 2] {
        if self.little_endian {
            val.to_le_bytes()
        } else {
            val.to_be_bytes()
        }
    }

    pub fn encode_u32(&self, val: u32) -> [u8; 4] {
        if self.little_endian {
            val.to_le_bytes()
        } else {
            val.to_be_bytes()
        }
    }

    pub fn abs_from_ifd_offset(&self, ifd_offset: u32) -> usize {
        self.base + ifd_offset as usize
    }

    pub fn read_ifd<R: Read + Seek>(
        &self,
        reader: &mut R,
        ifd_offset: u32,
    ) -> Result<Vec<RawEntry>, String> {
        let abs = self.abs_from_ifd_offset(ifd_offset);
        reader
            .seek(SeekFrom::Start(abs as u64))
            .map_err(|e| format!("TIFF: IFD seek to {abs}: {e}"))?;
        let mut count_buf = [0u8; 2];
        reader
            .read_exact(&mut count_buf)
            .map_err(|e| format!("TIFF: IFD count read at {abs}: {e}"))?;
        let count = self.decode_u16(&count_buf) as usize;
        if count > 1024 {
            return Err(format!("TIFF: implausible IFD entry count {count}"));
        }
        let mut buf = vec![0u8; count * 12];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("TIFF: IFD entries read at {abs}: {e}"))?;
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let chunk = &buf[i * 12..i * 12 + 12];
            entries.push(RawEntry {
                tag: self.decode_u16(&chunk[0..2]),
                field_type: self.decode_u16(&chunk[2..4]),
                count: self.decode_u32(&chunk[4..8]),
                value_bytes: chunk[8..12].try_into().unwrap(),
            });
        }
        Ok(entries)
    }

    pub fn find_entry<'b>(&self, entries: &'b [RawEntry], tag: u16) -> Option<&'b RawEntry> {
        entries.iter().find(|e| e.tag == tag)
    }

    pub fn entry_scalar(&self, e: &RawEntry) -> Option<u32> {
        if e.count != 1 {
            return None;
        }
        match e.field_type {
            TYPE_SHORT => Some(self.decode_u16(&e.value_bytes[..2]) as u32),
            TYPE_LONG => Some(self.decode_u32(&e.value_bytes)),
            _ => None,
        }
    }

    pub fn entry_long_array<R: Read + Seek>(
        &self,
        reader: &mut R,
        e: &RawEntry,
    ) -> Result<Vec<u32>, String> {
        if e.field_type != TYPE_LONG {
            return Err(format!("TIFF: expected LONG type, got {}", e.field_type));
        }
        let n = e.count as usize;
        if n == 0 {
            return Ok(Vec::new());
        }
        if n == 1 {
            return Ok(vec![self.decode_u32(&e.value_bytes)]);
        }
        let array_offset = self.decode_u32(&e.value_bytes);
        let abs = self.abs_from_ifd_offset(array_offset);
        reader
            .seek(SeekFrom::Start(abs as u64))
            .map_err(|e| format!("TIFF: LONG array seek to {abs}: {e}"))?;
        let mut buf = vec![0u8; n * 4];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("TIFF: LONG array read at {abs}: {e}"))?;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            out.push(self.decode_u32(&buf[i * 4..i * 4 + 4]));
        }
        Ok(out)
    }

    pub fn read_bytes<R: Read + Seek>(
        &self,
        reader: &mut R,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, String> {
        reader
            .seek(SeekFrom::Start(offset))
            .map_err(|e| format!("TIFF: seek to {offset}: {e}"))?;
        let mut buf = vec![0u8; length];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("TIFF: read {length} bytes at {offset}: {e}"))?;
        Ok(buf)
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddedJpeg {
    pub offset: u64,
    pub length: u32,
    pub width: u32,
    pub height: u32,
}

pub fn read_jpeg_sof<R: Read + Seek>(
    reader: &mut R,
    offset: usize,
    length: usize,
) -> Result<(u32, u32), String> {
    const SOF_WINDOW: usize = 4 * 1024;
    let to_read = length.min(SOF_WINDOW);
    reader
        .seek(SeekFrom::Start(offset as u64))
        .map_err(|e| format!("JPEG: seek to {offset}: {e}"))?;
    let mut buf = vec![0u8; to_read];
    reader
        .read_exact(&mut buf)
        .map_err(|e| format!("JPEG: read at {offset}: {e}"))?;
    let slice = &buf[..];
    if slice.len() < 4 || slice[0] != 0xFF || slice[1] != 0xD8 {
        return Err(format!("JPEG: no SOI at offset {offset}"));
    }
    let mut i = 2usize;
    while i + 1 < slice.len() {
        while i < slice.len() && slice[i] == 0xFF {
            i += 1;
        }
        if i >= slice.len() {
            break;
        }
        let marker = slice[i];
        i += 1;
        if matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        ) {
            if i + 7 > slice.len() {
                return Err("JPEG: SOF truncated".into());
            }
            let h = u16::from_be_bytes([slice[i + 3], slice[i + 4]]) as u32;
            let w = u16::from_be_bytes([slice[i + 5], slice[i + 6]]) as u32;
            return Ok((w, h));
        }
        if matches!(marker, 0xD8 | 0xD9 | 0xD0..=0xD7 | 0x01) {
            continue;
        }
        if i + 2 > slice.len() {
            return Err("JPEG: segment len OOB".into());
        }
        let seg_len = u16::from_be_bytes([slice[i], slice[i + 1]]) as usize;
        if seg_len < 2 {
            return Err("JPEG: seg_len < 2".into());
        }
        i = i
            .checked_add(seg_len)
            .ok_or_else(|| "JPEG: seg_len overflow".to_string())?;
    }
    Err("JPEG: SOF not found".into())
}
