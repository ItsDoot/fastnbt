use flate2::read::ZlibDecoder;
use std::convert::TryFrom;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use num_enum::TryFromPrimitive;

use crate::{Error, Result};

/// the size in bytes of a 'sector' in a region file. Sectors are Minecraft's size unit
/// for chunks. For example, a chunk might be `3 * SECTOR_SIZE` bytes. The
/// actual compressed bytes of a chunk may be smaller and the exact value is
/// tracking in the chunk header.
pub(crate) const SECTOR_SIZE: usize = 4096;

/// the size of the region file header.
pub(crate) const REGION_HEADER_SIZE: usize = 2 * SECTOR_SIZE;

/// size of header for each chunk in the region file. This header proceeds the
/// compressed chunk data.
pub(crate) const CHUNK_HEADER_SIZE: usize = 5;

pub trait RegionRead {
    fn read_chunk(&mut self, x: usize, z: usize) -> Result<Vec<u8>> {
        // Metadata encodes the length in bytes and the compression type
        let (scheme, compressed) = self.read_compressed_chunk(x, z)?;
        let compressed = Cursor::new(compressed);

        let mut decoder = match scheme {
            CompressionScheme::Zlib => ZlibDecoder::new(compressed),
            _ => panic!("unknown compression scheme (gzip?)"),
        };

        let mut outbuf = Vec::new();
        // read the whole Chunk
        decoder.read_to_end(&mut outbuf)?;
        Ok(outbuf)
    }

    /// Low level method. Read a compressed chunk, returning the compression
    /// scheme used.
    fn read_compressed_chunk(&mut self, x: usize, z: usize)
        -> Result<(CompressionScheme, Vec<u8>)>;

    // TODO: Let user provide a writer that we write the chunk directly to?
    // To be useful we would need a method that gets the compression scheme separately.
}

pub trait RegionWrite {
    /// Low level method. Write a chunk to the region file that has already been
    /// appropriately compressed for storage.
    fn write_compressed_chunk(
        &mut self,
        x: usize,
        z: usize,
        scheme: CompressionScheme,
        compressed_chunk: &[u8],
    ) -> Result<()>;
}

/// A Minecraft Region.
pub struct RegionBuffer<S> {
    data: S,
    // last offset is always the next valid place to write a chunk.
    offsets: Vec<u64>,
}

impl<S> RegionBuffer<S>
where
    S: Read + Write + Seek,
{
    pub fn new_empty(mut buf: S) -> Result<Self> {
        buf.rewind()?;
        buf.write_all(&[0; REGION_HEADER_SIZE])?;

        Ok(Self {
            data: buf,
            offsets: vec![2], // 2 is the end of the header
        })
    }

    /// Return the inner buffer used. The buffer is rewound to the beginning.
    pub fn into_inner(mut self) -> io::Result<S> {
        self.data.rewind()?;
        Ok(self.data)
    }

    fn header_pos(&self, x: usize, z: usize) -> u64 {
        (4 * ((x % 32) + (z % 32) * 32)) as u64
    }

    pub fn location(&mut self, x: usize, z: usize) -> io::Result<ChunkLocation> {
        self.data.seek(SeekFrom::Start(self.header_pos(x, z)))?;

        let mut buf = [0u8; 4];
        self.data.read_exact(&mut buf[..])?;

        let mut offset = 0u64;
        offset |= (buf[0] as u64) << 16;
        offset |= (buf[1] as u64) << 8;
        offset |= buf[2] as u64;
        let sectors = buf[3] as u64;

        Ok(ChunkLocation { offset, sectors })
    }

    fn set_chunk(&mut self, offset: u64, scheme: CompressionScheme, chunk: &[u8]) -> Result<()> {
        self.data
            .seek(SeekFrom::Start(offset * SECTOR_SIZE as u64))?;

        self.data.write_all(&self.chunk_meta(
            chunk.len() as u32, // doesn't include header size
            scheme,
        ))?;

        self.data.write_all(chunk)?;
        Ok(())
    }

    fn set_header(
        &mut self,
        x: usize,
        z: usize,
        offset: u64,
        new_sector_count: usize,
    ) -> Result<()> {
        if new_sector_count > 255 {
            return Err(Error::ChunkTooLarge);
        }

        let mut buf = [0u8; 4];
        buf[0] = ((offset & 0xFF0000) >> 16) as u8;
        buf[1] = ((offset & 0x00FF00) >> 8) as u8;
        buf[2] = (offset & 0x0000FF) as u8;
        buf[3] = new_sector_count as u8; // TODO, what if it doesn't fit.

        // seek to header
        self.data.seek(SeekFrom::Start(self.header_pos(x, z)))?;
        self.data.write_all(&buf)?;
        Ok(())
    }

    fn chunk_meta(&self, compressed_chunk_size: u32, scheme: CompressionScheme) -> [u8; 5] {
        let mut buf = [0u8; 5];
        let mut c = Cursor::new(buf.as_mut_slice());

        // size written to disk includes the byte representing the compression
        // scheme, so +1.
        c.write_u32::<BigEndian>(compressed_chunk_size + 1).unwrap();
        c.write_u8(match scheme {
            CompressionScheme::Gzip => 1,
            CompressionScheme::Zlib => 2,
            CompressionScheme::Uncompressed => 3,
        })
        .unwrap();

        buf
    }
}

impl<S> RegionRead for RegionBuffer<S>
where
    S: Read + Write + Seek,
{
    fn read_compressed_chunk(
        &mut self,
        x: usize,
        z: usize,
    ) -> Result<(CompressionScheme, Vec<u8>)> {
        if x >= 32 || z >= 32 {
            return Err(Error::InvalidOffset(x as isize, z as isize));
        }

        let loc = self.location(x, z)?;

        if loc.offset == 0 && loc.sectors == 0 {
            Err(Error::ChunkNotFound)
        } else {
            self.data
                .seek(SeekFrom::Start(loc.offset * SECTOR_SIZE as u64))?;

            let mut buf = [0u8; 5];
            self.data.read_exact(&mut buf)?;
            let metadata = ChunkMeta::new(&buf)?;

            let mut compressed_chunk = vec![0; metadata.compressed_len as usize];
            self.data.read_exact(&mut compressed_chunk)?;

            Ok((metadata.compression_scheme, compressed_chunk))
        }
    }
}

impl<S> RegionWrite for RegionBuffer<S>
where
    S: Seek + Write + Read,
{
    fn write_compressed_chunk(
        &mut self,
        x: usize,
        z: usize,
        scheme: CompressionScheme,
        chunk: &[u8],
    ) -> Result<()> {
        let loc = self.location(x, z)?;
        let required_sectors = unstable_div_ceil(CHUNK_HEADER_SIZE + chunk.len(), SECTOR_SIZE);

        if loc.offset == 0 && loc.sectors == 0 {
            // chunk does not exist in the region yet.
            let offset = *self.offsets.last().expect("offset should always exist");

            // add a new offset representing the new 'end' of the current region file.
            self.offsets.push(offset + required_sectors as u64);
            self.set_chunk(offset, scheme, chunk)?;
            self.set_header(x, z, offset, required_sectors)?;
        } else {
            // chunk already exists in the region file, need to update it.
            let i = self.offsets.binary_search(&loc.offset).unwrap();
            let start_offset = self.offsets[i];
            let end_offset = self.offsets[i + 1];
            let available_sectors = (end_offset - start_offset) as usize;

            if required_sectors <= available_sectors {
                // we fit in the current gap in the file.
                self.set_chunk(start_offset, scheme, chunk)?;
                self.set_header(x, z, start_offset, required_sectors)?;
            } else {
                // we do not fit in the current gap, need to find a new home for
                // this chunk.
                self.offsets.remove(i); // this chunk will no longer be here.
                let offset = *self.offsets.last().unwrap() as u64;

                // add a new offset representing the new 'end' of the current region file.
                self.offsets.push(offset + required_sectors as u64);
                self.set_chunk(offset, scheme, chunk)?;
                self.set_header(x, z, offset, required_sectors)?;
            }
        }

        Ok(())
    }
}

/// Various compression schemes that NBT data is typically compressed with.
#[derive(Debug, TryFromPrimitive)]
#[repr(u8)]
pub enum CompressionScheme {
    Gzip = 1,
    Zlib = 2,
    Uncompressed = 3,
}

impl<S: Seek + Read + Write> RegionBuffer<S> {
    pub fn new(data: S) -> Result<Self> {
        let mut tmp = Self {
            data,
            offsets: vec![],
        };

        let mut max_offset = 0;
        let mut max_offsets_sector_count = 0;

        for z in 0..32 {
            for x in 0..32 {
                let loc = tmp.location(x, z)?;
                if loc.offset == 0 && loc.sectors == 0 {
                    continue;
                }

                tmp.offsets.push(loc.offset);
                if loc.offset > max_offset {
                    max_offset = loc.offset;
                    max_offsets_sector_count = loc.sectors;
                }
            }
        }

        tmp.offsets.sort_unstable();

        // we add an offset representing the end of sectors that are in use.
        tmp.offsets.push(max_offset + max_offsets_sector_count);
        Ok(tmp)
    }
}

// copied from rust std unstable_div_ceil function
pub const fn unstable_div_ceil(lhs: usize, rhs: usize) -> usize {
    let d = lhs / rhs;
    let r = lhs % rhs;
    if r > 0 && rhs > 0 {
        d + 1
    } else {
        d
    }
}

#[derive(Debug)]
pub struct ChunkLocation {
    /// The offset, in units of 4kiB sectors, into the region file this chunk is
    /// located at. Offset 0 is the start of the file.
    pub offset: u64,

    /// The number of 4 kiB sectors that this chunk occupies in the region file.
    pub sectors: u64,
}

/// Encodes how the NBT-Data is compressed
#[derive(Debug)]
struct ChunkMeta {
    pub compressed_len: u32,
    pub compression_scheme: CompressionScheme,
}

impl ChunkMeta {
    fn new(data: &[u8]) -> Result<Self> {
        if data.len() < 5 {
            return Err(Error::InsufficientData);
        }

        let mut buf = &data[..5];
        let len = buf.read_u32::<BigEndian>()?;
        let scheme = buf.read_u8()?;
        let scheme = CompressionScheme::try_from(scheme).map_err(|_| Error::InvalidChunkMeta)?;

        Ok(Self {
            compressed_len: len - 1, // this len include the compression byte.
            compression_scheme: scheme,
        })
    }
}
