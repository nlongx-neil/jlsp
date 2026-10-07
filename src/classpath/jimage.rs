use std::fs::File;
use std::path::Path;
use byteorder::{BigEndian, LittleEndian, ReadBytesExt};

const JIMAGE_HEADER_MAGIC: u32 = 0xCAFE_DADA;

/// Very basic reader for Java 9+ `lib/modules` (jimage format)
/// Format: Magic(4) Major(2) Minor(2) Flags(4) NodeCount(4) LocationCount(4) StringsSize(4) IndexSize(4)
#[allow(dead_code)]
pub struct JImageReader {
    file: File,
    endian_is_be: bool,
    node_count: u32,
    location_count: u32,
    strings_size: u32,
    index_size: u32,
}

impl JImageReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let mut file = File::open(&path).map_err(|e| format!("Failed to open modules: {e}"))?;
        let magic = file.read_u32::<BigEndian>().map_err(|e| e.to_string())?;

        let (_magic, is_be) = if magic == JIMAGE_HEADER_MAGIC {
            (magic, true)
        } else if magic.swap_bytes() == JIMAGE_HEADER_MAGIC {
            (magic.swap_bytes(), false)
        } else {
            return Err("Not a valid JImage file".into());
        };

        let (_major, _minor, _flags, node_count, location_count, strings_size, index_size) = if is_be {
            (
                file.read_u16::<BigEndian>().map_err(|e| e.to_string())?,
                file.read_u16::<BigEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<BigEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<BigEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<BigEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<BigEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<BigEndian>().map_err(|e| e.to_string())?,
            )
        } else {
            (
                file.read_u16::<LittleEndian>().map_err(|e| e.to_string())?,
                file.read_u16::<LittleEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<LittleEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<LittleEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<LittleEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<LittleEndian>().map_err(|e| e.to_string())?,
                file.read_u32::<LittleEndian>().map_err(|e| e.to_string())?,
            )
        };

        Ok(Self {
            file,
            endian_is_be: is_be,
            node_count,
            location_count,
            strings_size,
            index_size,
        })
    }
}
