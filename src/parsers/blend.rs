use crate::parsers::{
    dna::{Dna, DnaParseContext},
    BlendParseError, Endianness, PointerSize, Result,
};
use nom::{
    branch::alt,
    bytes::complete::{tag, take},
    multi::many_till,
    number::complete::{be_u32, be_u64, le_u32, le_u64},
    sequence::tuple,
    Err,
};
use std::{
    convert::TryInto,
    fmt::{self, Debug, Formatter},
    io::Read,
    num::NonZeroU64,
    path::Path,
    result::Result as StdResult,
};

pub struct BlockData {
    /// The entire binary data of the `Block` in the blend file.
    pub data: Vec<u8>,
    /// The data field can contain more than one struct, count tells us how many there is.
    pub count: usize,
}

impl Debug for BlockData {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "len/count: {}/{}", self.data.len(), self.count)
    }
}

/// Represents all possible block types found in the blend file.
/// `Rend`, `Test` and `Global` are ignored by this crate but are still represented here.
#[derive(Debug)]
pub enum Block {
    Rend,
    Test,
    Global {
        memory_address: NonZeroU64,
        dna_index: usize,
        data: BlockData,
    },
    /// A principal (or root) block is defined by having a two digit code and by the fact that its `dna_index` is always
    /// valid. If we have a pointer to a principal block, we can ignore the type of the pointer and use the block type.
    Principal {
        code: [u8; 2],
        memory_address: NonZeroU64,
        dna_index: usize,
        data: BlockData,
    },
    /// Subsidiary blocks are defined by having the code "DATA", which is ommited here. Their `dna_index` is not
    /// always correct and is only used when whichever field points to them has an "invalid" type (like void*).
    Subsidiary {
        memory_address: NonZeroU64,
        dna_index: usize,
        data: BlockData,
    },
    /// The DNA of the blend file. Used to interpret all the other blocks.
    Dna(Dna),
}

/// The on-disk layout of a blend file.
///
/// Blender 5.0 introduced a new file/block header format to support data blocks larger than 2GiB.
/// The two variants are not binary compatible so the parser needs to know which one it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    /// Pre-5.0 format: 12-byte file header and a block header of
    /// `code(4) + size(u32) + address(ptr) + sdna_index(u32) + count(u32)`.
    Legacy,
    /// Blender 5.0+ format: 17-byte file header and a block header of
    /// `code(4) + sdna_index(u32) + address(u64) + size(u64) + count(u64)`.
    New,
}

/// The version of Blender used to save the blend file.
///
/// Blender stores the version as a single integer of the form `major * 100 + minor` (the same
/// convention as `BLENDER_VERSION` in Blender's own source). Legacy files encode this as three
/// ASCII digits (e.g. `"280"` -> 2.80) and Blender 5.0+ files as four (e.g. `"0501"` -> 5.1), so
/// splitting into `major`/`minor` keeps the value correct regardless of how many digits are used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub major: u16,
    pub minor: u16,
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl Version {
    /// Parses ASCII version digits (`b"280"`, `b"0501"`, ...) into a `major`/`minor` pair.
    fn from_digits(digits: &[u8]) -> Version {
        let combined = digits
            .iter()
            .filter(|b| b.is_ascii_digit())
            .fold(0_u16, |acc, &b| acc * 10 + u16::from(b - b'0'));

        Version {
            major: combined / 100,
            minor: combined % 100,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    /// The size of the pointer on the machine used to save the blend file.
    pub pointer_size: PointerSize,
    /// The endianness on the machine used to save the blend file.
    pub endianness: Endianness,
    /// The version of Blender used to save the blend file.
    pub version: Version,
    /// The on-disk layout of the file, which controls how block headers are parsed.
    pub format: FileFormat,
}

fn pointer_size_bits32(input: &[u8]) -> Result<'_, PointerSize> {
    let (input, _) = tag("_")(input)?;
    Ok((input, PointerSize::Bits32))
}

fn pointer_size_bits64(input: &[u8]) -> Result<'_, PointerSize> {
    let (input, _) = tag("-")(input)?;
    Ok((input, PointerSize::Bits64))
}

pub fn pointer_size(input: &[u8]) -> Result<'_, PointerSize> {
    alt((pointer_size_bits32, pointer_size_bits64))(input)
}

fn endianness_litte(input: &[u8]) -> Result<'_, Endianness> {
    let (input, _) = tag("v")(input)?;
    Ok((input, Endianness::Little))
}

fn endianness_big(input: &[u8]) -> Result<'_, Endianness> {
    let (input, _) = tag("V")(input)?;
    Ok((input, Endianness::Big))
}

pub fn endianness(input: &[u8]) -> Result<'_, Endianness> {
    alt((endianness_litte, endianness_big))(input)
}

pub fn version(input: &[u8]) -> Result<'_, Version> {
    let (input, v) = take(3_usize)(input)?;
    Ok((input, Version::from_digits(v)))
}

pub fn header(input: &[u8]) -> Result<'_, Header> {
    let (input, _) = match tag::<_, _, BlendParseError>("BLENDER")(input) {
        Ok(v) => v,
        Err(_) => {
            return Err(nom::Err::Failure(
                BlendParseError::CompressedFileNotSupported,
            ))
        }
    };

    // Detect the file format. In the legacy (pre-5.0) header the byte right after "BLENDER" is the
    // pointer-size marker ('_' or '-'). In the Blender 5.0+ header it is a two-digit ASCII number
    // encoding the total header size (e.g. "17"), so it is always a digit here.
    if let Some(b'_') | Some(b'-') = input.first() {
        let (input, (pointer_size, endianness, version)) =
            tuple((pointer_size, endianness, version))(input)?;

        Ok((
            input,
            Header {
                pointer_size,
                endianness,
                version,
                format: FileFormat::Legacy,
            },
        ))
    } else {
        // New 17-byte header (Blender 5.0+):
        //   "BLENDER" + header_size(2 digits) + pointer_size(1) + format_version(2 digits)
        //             + endianness(1) + file_version(4 digits)
        // The pointer-size ('-') and endianness ('v') markers are kept only for readability and are
        // always 64-bit little-endian in this format, but we still parse them from their positions.
        let (input, _header_size) = take(2_usize)(input)?;
        let (input, pointer_size) = pointer_size(input)?;
        let (input, _format_version) = take(2_usize)(input)?;
        let (input, endianness) = endianness(input)?;
        let (input, v) = take(4_usize)(input)?;

        // The version is now four digits (upper two = major, lower two = minor), e.g. "0501" -> 5.1.
        let version = Version::from_digits(v);

        Ok((
            input,
            Header {
                pointer_size,
                endianness,
                version,
                format: FileFormat::New,
            },
        ))
    }
}

pub fn block_header_code(input: &[u8]) -> Result<'_, [u8; 4]> {
    let (input, v) = take(4_usize)(input)?;
    Ok((input, [v[0], v[1], v[2], v[3]]))
}

#[derive(Debug)]
pub struct RawBlend {
    pub header: Header,
    pub blocks: Vec<Block>,
    pub dna: Dna,
}

impl RawBlend {
    /// Returns a new `Blend` instance from `data`.
    pub fn from_data<T: Read>(mut data: T) -> StdResult<Self, BlendParseError> {
        let mut buffer = Vec::new();
        data.read_to_end(&mut buffer)
            .map_err(BlendParseError::IoError)?;

        let mut parser = BlendParseContext::default();
        let res = parser.blend(&buffer);

        match res {
            Ok((_, blend)) => Ok(blend),
            Err(Err::Failure(e)) | Err(Err::Error(e)) => Err(e),
            Err(Err::Incomplete(..)) => Err(BlendParseError::NotEnoughData),
        }
    }

    /// Returns a new `Blend` instance from a path.
    pub fn from_path<P: AsRef<Path>>(path: P) -> StdResult<Self, BlendParseError> {
        use std::fs::File;

        let file = File::open(path).map_err(BlendParseError::IoError)?;
        RawBlend::from_data(file)
    }
}

#[derive(Default)]
pub enum BlendParseContext {
    #[default]
    Empty,
    ParsedHeader(Header),
}

impl BlendParseContext {
    fn memory_address<'a>(&self, input: &'a [u8]) -> Result<'a, NonZeroU64> {
        match self {
            BlendParseContext::ParsedHeader(header) => {
                let read_len: usize = match header.pointer_size {
                    PointerSize::Bits32 => 4,
                    PointerSize::Bits64 => 8,
                };

                let (input, data) = take(read_len)(input)?;

                let (_, address) = match (&header.endianness, &header.pointer_size) {
                    (Endianness::Little, PointerSize::Bits32) => {
                        le_u32(data).map(|(i, n)| (i, u64::from(n)))?
                    }
                    (Endianness::Big, PointerSize::Bits32) => {
                        be_u32(data).map(|(i, n)| (i, u64::from(n)))?
                    }
                    (Endianness::Little, PointerSize::Bits64) => le_u64(data)?,
                    (Endianness::Big, PointerSize::Bits64) => be_u64(data)?,
                };

                if let Some(address) = NonZeroU64::new(address) {
                    Ok((input, address))
                } else {
                    Err(Err::Failure(BlendParseError::InvalidMemoryAddress))
                }
            }
            BlendParseContext::Empty => unreachable!("Header should be parsed here"),
        }
    }

    /// Panics if a u32 can't be converted to usize in your system.
    fn block<'a, 'b>(&'a self, input: &'b [u8]) -> Result<'b, Block>
    where
        'b: 'a,
    {
        match self {
            BlendParseContext::ParsedHeader(header) => {
                let (input, code) = block_header_code(input)?;

                // The block header layout changed in Blender 5.0. See `FileFormat` for details.
                let (input, size, memory_address, dna_index, count): (_, usize, _, u32, u64) =
                    match header.format {
                        FileFormat::Legacy => {
                            // code(4) + size(u32) + address(ptr) + sdna_index(u32) + count(u32)
                            let (input, size): (_, usize) = match header.endianness {
                                Endianness::Little => le_u32(input)
                                    .map(|(i, n)| (i, n.try_into().expect("u32 to usize")))?,
                                Endianness::Big => be_u32(input)
                                    .map(|(i, n)| (i, n.try_into().expect("u32 to usize")))?,
                            };
                            let (input, memory_address) = self.memory_address(input)?;
                            let (input, dna_index) = match header.endianness {
                                Endianness::Little => le_u32(input)?,
                                Endianness::Big => be_u32(input)?,
                            };
                            let (input, count) = match header.endianness {
                                Endianness::Little => le_u32(input)?,
                                Endianness::Big => be_u32(input)?,
                            };
                            (input, size, memory_address, dna_index, u64::from(count))
                        }
                        FileFormat::New => {
                            // code(4) + sdna_index(u32) + address(u64) + size(u64) + count(u64),
                            // always little-endian and 64-bit.
                            let (input, dna_index) = le_u32(input)?;
                            let (input, memory_address) = self.memory_address(input)?;
                            let (input, size) = le_u64(input)?;
                            let (input, count) = le_u64(input)?;
                            let size: usize = size.try_into().expect("u64 to usize");
                            (input, size, memory_address, dna_index, count)
                        }
                    };

                let (input, block_data) = take(size)(input)?;

                //Assumption: These block codes will always exist
                let block = match &code {
                    b"REND" => Block::Rend,
                    b"TEST" => Block::Test,
                    b"GLOB" => Block::Global {
                        memory_address,
                        dna_index: dna_index.try_into().expect("u32 to usize"),
                        data: BlockData {
                            data: block_data.to_vec(),
                            count: count.try_into().expect("u32 to usize"),
                        },
                    },
                    b"DATA" => Block::Subsidiary {
                        memory_address,
                        dna_index: dna_index.try_into().expect("u32 to usize"),
                        data: BlockData {
                            data: block_data.to_vec(),
                            count: count.try_into().expect("u32 to usize"),
                        },
                    },
                    b"DNA1" => {
                        let ctx = DnaParseContext::new(header.endianness, header.pointer_size);
                        let (_, dna) = ctx.dna(block_data)?;

                        Block::Dna(dna)
                    }
                    &[code1, code2, 0, 0] => {
                        if count != 1 {
                            return Err(Err::Failure(
                                BlendParseError::UnsupportedCountOnPrincipalBlock,
                            ));
                        } else {
                            Block::Principal {
                                code: [code1, code2],
                                memory_address,
                                dna_index: dna_index.try_into().expect("u32 to usize"),
                                data: BlockData {
                                    data: block_data.to_vec(),
                                    count: 1,
                                },
                            }
                        }
                    }
                    _ => return Err(Err::Failure(BlendParseError::UnknownBlockCode)),
                };

                Ok((input, block))
            }
            BlendParseContext::Empty => unreachable!("Header should be parsed here"),
        }
    }

    pub fn blend<'a, 'b>(&'a mut self, input: &'b [u8]) -> Result<'b, RawBlend>
    where
        'b: 'a,
    {
        let (input, header) = header(input)?;

        //This has to happen before the rest of the parser runs
        *self = BlendParseContext::ParsedHeader(header.clone());

        let (input, (mut blocks, _)) = many_till(move |d| self.block(d), tag("ENDB"))(input)?;

        let dna = if let Some(Block::Dna(dna)) = blocks.pop() {
            // Assumption: The DNA block is always the last one
            dna
        } else {
            return Err(Err::Failure(BlendParseError::NoDnaBlockFound));
        };

        Ok((
            input,
            RawBlend {
                blocks,
                dna,
                header,
            },
        ))
    }
}
