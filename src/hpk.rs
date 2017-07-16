extern crate byteorder;
extern crate libflate;

use ::errors::*;
use std::io;
use std::io::prelude::*;
use std::io::BufReader;
use std::io::SeekFrom;
use std::fs;
use std::collections::HashMap;
use self::byteorder::{ByteOrder, LittleEndian};

const FILE_ENTRY_SIZE: usize = 8;
const NAME_ENTRY_MIN_SIZE: usize = 10;

const ZLIB_BLOCKTBL_OFFSET: u64 = 0x0c;
const ZLIB_MAX_CACHE_ENTRIES: usize = 2;
const ZLIB_MAX_BLOCKSIZE: u64 = 0x1000000;


pub enum EntryType {
    File,
    Directory
}

struct NameTableEntry {
    file_index: u32,
    entry_type: EntryType,
    entry_size: u32,
    name: String
}

struct FileTableEntry {
    offset: u32,
    size: u32
}

pub struct File {
    name_entry: NameTableEntry,
    file_entry: FileTableEntry
}

pub struct Directory {
    files: Vec<File>,
    directories: Vec<Directory>,
    name_entry: Option<NameTableEntry>,
    file_entry: FileTableEntry
}

struct ArchiveFile {
    filetbl_offset: u64,
    reader: BufReader<fs::File>,
    basefile: fs::File
}

pub struct Archive {
    file: ArchiveFile,
    rootdir: Directory,
}

enum FileDataEncoding {
    Plain(FileDataPlain),
    Zlib(FileDataZlib)
}

struct FileDataPlain {
    file: fs::File,
    size: u64,
    base_offset: u64,
    cur_offset: u64,
}

struct FileDataZlib {
    plain: FileDataPlain,
    size: u64,
    cur_offset: u64,
    blocksize: u64,
    cache: HashMap<u32, Vec<u8>>
}

pub struct FileData {
    fdata: FileDataEncoding,
}

impl File {
    pub fn name(&self) -> &str
    {
        &self.name_entry.name
    }

    pub fn size(&self) -> u32
    {
        self.file_entry.size
    }
}

impl Directory {
    pub fn files(&self) -> &Vec<File>
    {
        &self.files
    }

    pub fn directories(&self) -> &Vec<Directory>
    {
        &self.directories
    }

    pub fn name(&self) -> Option<&str>
    {
        return match self.name_entry {
            None => None,
            Some(ref ne) => Some(&ne.name)
        }
    }
}

impl FileDataPlain {
    fn from(mut file: fs::File, fentry: &FileTableEntry) -> Result<FileDataPlain>
    {
        Ok(FileDataPlain {
            file: file,
            size: fentry.size as u64,
            base_offset: fentry.offset as u64,
            cur_offset: 0,
        })
    }

    fn size(&self) -> u64
    {
        return self.size;
    }
}

impl Read for FileDataPlain {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>
    {
        let mut readable: usize =
            self.size as usize - self.cur_offset as usize;
        if readable > buf.len() {
            readable = buf.len();
        };
        let readlen = self.file.read(&mut buf[..readable])?;
        self.cur_offset += readlen as u64;
        Ok(readlen)
    }
}

impl Seek for FileDataPlain {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64>
    {
        use std::io::{Error, ErrorKind};
        match style {
            SeekFrom::Start(o) => {
                if o > self.size {
                    Err(io::Error::new(ErrorKind::InvalidData,
                                       "Attempted to seek beyond EOF"))
                } else {
                    let new_off = self.file.seek(SeekFrom::Start(self.base_offset + o))?;
                    self.cur_offset = new_off - self.base_offset;
                    Ok(self.cur_offset)
                }
            },
            SeekFrom::End(o) => {
                let wanted_off = (self.size as i64) + o;
                if o > 0 {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Attempted to seek beyond EOF"))
                } else if wanted_off < 0 {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Seek resulted in negative offset"))
                } else {
                    let new_off = self.file.seek(
                        SeekFrom::Start(
                            self.base_offset + wanted_off as u64))?;
                    self.cur_offset = new_off - self.base_offset;
                    Ok(self.cur_offset)
                }
            },
            SeekFrom::Current(o) => {
                let cur = self.cur_offset as i64;
                let wanted_off = cur + o;
                if wanted_off < 0 {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Seek resulted in negative offset"))
                } else if wanted_off > (self.size as i64) {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Attempted to seek beyond EOF"))
                } else {
                    let new_off = self.file.seek(
                        SeekFrom::Start(self.base_offset + wanted_off as u64))?;
                    self.cur_offset = new_off - self.base_offset;
                    Ok(self.cur_offset)
                }
            }
        }
    }
}

impl FileDataZlib {
    fn parse_header(header: &[u8]) -> Result<(u64, u64)>
    {
        let mut magic_iter = (&header[0..4]).into_iter();
        if !"ZLIB".bytes().all(|i1| {
            match magic_iter.next() {
                Some(i2) => &i1 == i2,
                None => false
            }
        }) {
            bail!("Invalid magic");
        }
        let size = LittleEndian::read_u32(&header[4..8]) as u64;
        let blocksize = LittleEndian::read_u32(&header[8..0xc]) as u64;
        if blocksize == 0 {
            bail!("Block size is 0");
        }
        if blocksize > ZLIB_MAX_BLOCKSIZE {
            bail!("Block size is exceeding the maximum allowed: {} > {}",
                  blocksize, ZLIB_MAX_BLOCKSIZE);
        }
        Ok((size, blocksize))
    }

    fn from(mut file: fs::File, fentry: &FileTableEntry) -> Result<FileDataZlib>
    {
        let mut plain = FileDataPlain::from(file, fentry)?;
        let expanded_size: u64;
        let blocksize: u64;
        let (expanded_size, blocksize) = {
            let mut header = [0u8; 0xc];
            plain.read_exact(&mut header)?;
            Self::parse_header(&header)?
        };

        Ok(FileDataZlib {
            plain: plain,
            size: expanded_size,
            blocksize: blocksize,
            cur_offset: 0u64,
            cache: HashMap::new(),
        })
    }

    fn size(&self) -> u64
    {
        return self.size;
    }

    /** Evict one entry from the cache, provided that it is not idx.
     * Panics if idx is the only entry in the cache or if no entry can be
     * evicted. */
    fn evict_another_entry(&mut self, idx: u32)
    {
        if self.cache.len() == 0 {
            panic!("Cannot evict an entry from an empty cache!");
        }
        if self.cache.len() == 1 && self.cache.contains_key(&idx) {
            panic!("Cannot evict the only entry we try to keep in the cache!");
        }
        let min = *self.cache.keys().min().unwrap();
        if min == idx {
            let max = *self.cache.keys().max().unwrap();
            self.cache.remove(&max);
        } else {
            self.cache.remove(&min);
        }
    }

    fn read_block_offset_and_size(&mut self, idx: u32) -> io::Result<(u64, u64, u64)>
    {
        let partial_block_size = (self.size % self.blocksize) as u64;
        let num_blocks = if partial_block_size > 0 {
            ((self.size / self.blocksize) as u32) + 1
        } else {
            (self.size / self.blocksize) as u32
        };
        if idx >= num_blocks {
            panic!("idx {} is higher than the total number of blocks ({})",
                   idx, num_blocks);
        }
        if num_blocks == 0 {
            return Ok((ZLIB_BLOCKTBL_OFFSET, 0u64, 0u64));
        }

        let last_block = num_blocks - 1;
        let start_off = {
            let mut buf = [0u8; 4];
            let tbl_entry_off = ZLIB_BLOCKTBL_OFFSET + (idx as u64 * 4);
            self.plain.seek(SeekFrom::Start(tbl_entry_off))?;
            self.plain.read_exact(&mut buf)?;
            LittleEndian::read_u32(&buf[..]) as u64
        };
        let (end_off, unpacked_size) = if idx == last_block {
            (self.plain.size(), partial_block_size)
        } else {
            let mut buf = [0u8; 4];
            self.plain.read_exact(&mut buf)?;
            (LittleEndian::read_u32(&buf[..]) as u64, self.blocksize)
        };
        let size = end_off - start_off;
        if size > self.blocksize {
            use std::io::ErrorKind;
            let err = io::Error::new(ErrorKind::InvalidData,
                                     format!("Block at index {} is larger than block size ({} > {})",
                                     idx, size, self.blocksize));
            return Err(err);
        }
        Ok((start_off, size, unpacked_size))
    }

    /** Read and decompress a block. */
    fn read_block(&mut self, idx: u32) -> io::Result<Vec<u8>>
    {
        let (pack_start, pack_size, unpack_size) = self.read_block_offset_and_size(idx)?;
        let mut plain_block = vec![0u8; pack_size as usize];
        self.plain.seek(SeekFrom::Start(pack_start))?;
        self.plain.read_exact(&mut plain_block)?;
        if pack_size == unpack_size {
            return Ok(plain_block);
        };
        /* Pack size is lower than block size => pack is compressed */
        use self::libflate::zlib::Decoder;
        let mut decoder = Decoder::new(&plain_block[..])?;
        let mut inflated_block = vec![0u8; unpack_size as usize];
        decoder.read_exact(&mut inflated_block)?;
        Ok(inflated_block)
    }

    /** Get a block from the cache. If none exist, read the requested block and
     * add it into the cache. */
    fn get_block(&mut self, idx: u32) -> io::Result<&Vec<u8>>
    {
        if self.cache.contains_key(&idx) {
            return Ok(self.cache.get(&idx).unwrap());
        }

        let block = self.read_block(idx)?;
        while self.cache.len() >= ZLIB_MAX_CACHE_ENTRIES {
            self.evict_another_entry(idx);
        };
        self.cache.insert(idx, block);
        Ok(self.cache.get(&idx).unwrap())
    }
}

impl Read for FileDataZlib {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>
    {
        let mut out_pos = 0u64;
        let mut size_left = buf.len() as u64;
        if size_left > (self.size - self.cur_offset) {
            self.size - self.cur_offset;
        };
        while size_left > 0 && self.cur_offset < self.size {
            let idx = (self.cur_offset / self.blocksize) as u32;
            let block_offset = self.cur_offset % self.blocksize;
            let to_copy;
            {
                let blockdata = self.get_block(idx)?;
                to_copy = if size_left < (blockdata.len() as u64 - block_offset) {
                    size_left
                } else {
                    blockdata.len() as u64 - block_offset
                };
                &mut buf[out_pos as usize..(out_pos + to_copy) as usize].copy_from_slice(
                    &blockdata[block_offset as usize..(block_offset + to_copy) as usize]);
            }
            out_pos += to_copy;
            size_left -= to_copy;
            self.cur_offset += to_copy;
        }
        Ok(out_pos as usize)
    }
}

impl Seek for FileDataZlib {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64>
    {
        use std::io::{Error, ErrorKind};
        match style {
            SeekFrom::Start(o) => {
                if o > self.size {
                    Err(io::Error::new(ErrorKind::InvalidData,
                                       "Attempted to seek beyond EOF"))
                } else {
                    self.cur_offset = o;
                    Ok(self.cur_offset)
                }
            },
            SeekFrom::End(o) => {
                let wanted_off = (self.size as i64) + o;
                if o > 0 {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Attempted to seek beyond EOF"))
                } else if wanted_off < 0 {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Seek resulted in negative offset"))
                } else {
                    self.cur_offset = wanted_off as u64;
                    Ok(self.cur_offset)
                }
            },
            SeekFrom::Current(o) => {
                let cur = self.cur_offset as i64;
                let wanted_off = cur + o;
                if wanted_off < 0 {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Seek resulted in negative offset"))
                } else if wanted_off > (self.size as i64) {
                    Err(Error::new(ErrorKind::InvalidData,
                                   "Attempted to seek beyond EOF"))
                } else {
                    self.cur_offset = wanted_off as u64;
                    Ok(self.cur_offset)
                }
            }
        }
    }
}

impl FileData {
    fn new(mut file: fs::File, fentry: &FileTableEntry) -> Result<FileData>
    {
        file.seek(SeekFrom::Start(fentry.offset as u64))?;
        let is_zlib = {
            let mut magic = [0u8; 4];
            file.read_exact(&mut magic)?;
            file.seek(SeekFrom::Start(fentry.offset as u64))?;
            let mut magic_iter = magic.into_iter();
            "ZLIB".bytes().all(|i1| {
                match magic_iter.next() {
                    Some(i2) => &i1 == i2,
                    None => false
                }
            })
        };
        if is_zlib {
            Ok(FileData {
                fdata: FileDataEncoding::Zlib(FileDataZlib::from(file, fentry)?),
            })
        } else {
            Ok(FileData {
                fdata: FileDataEncoding::Plain(FileDataPlain::from(file, fentry)?),
            })
        }
    }

    pub fn size(&self) -> u64
    {
        match &self.fdata {
            &FileDataEncoding::Plain(ref plain) => plain.size(),
            &FileDataEncoding::Zlib(ref zlib) => zlib.size()
        }
    }
}

impl Read for FileData {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>
    {
        match &mut self.fdata {
            &mut FileDataEncoding::Plain(ref mut plain) => plain.read(buf),
            &mut FileDataEncoding::Zlib(ref mut zlib) => zlib.read(buf)
        }
    }
}

impl Seek for FileData {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64>
    {
        match &mut self.fdata {
            &mut FileDataEncoding::Plain(ref mut plain) => plain.seek(style),
            &mut FileDataEncoding::Zlib(ref mut zlib) => zlib.seek(style)
        }
    }
}

impl ArchiveFile {

    fn read_header<T: Read+Seek>(reader: &mut T) -> Result<u32>
    {
        let header_size;
        let magic;
        let filetbl_offset;
        reader.seek(SeekFrom::Start(0))?;
        {
            let mut buf = [0u8; 0x20];
            reader.read_exact(&mut buf)?;
            magic = LittleEndian::read_u32(&buf[0..4]);
            header_size = LittleEndian::read_u32(&buf[4..8]);
            filetbl_offset = LittleEndian::read_u32(&buf[0x1c..0x20]);
        }
        if magic != 0x4c555042 {
            bail!("Invalid magic");
        }
        if header_size < 0x20 {
            bail!("Header size too short");
        }
        if header_size > 0x24 {
            bail!("Unsupported format variant: 0x{:x}", header_size);
        }
        if filetbl_offset < header_size {
            bail!("File table and file header are overlapping");
        }
        Ok(filetbl_offset)
    }

    fn read_file_entry(&mut self, mut index: u32) -> Result<FileTableEntry>
    {
        let offset;
        let size;
        if index == 0 {
            bail!("Index cannot be 0");
        }
        // Index is 1 based
        index = index - 1;
        let entry_offset = self.filetbl_offset + (index as u64 * FILE_ENTRY_SIZE as u64);
        self.reader.seek(SeekFrom::Start(entry_offset))?;
        {
            let mut buf = [0; FILE_ENTRY_SIZE];
            self.reader.read_exact(&mut buf)?;
            offset = LittleEndian::read_u32(&buf[0..4]);
            size = LittleEndian::read_u32(&buf[4..8]);
        }
        Ok(FileTableEntry {
            offset: offset,
            size: size
        })
    }

    fn read_name_entry(&mut self, offset: u64) -> Result<NameTableEntry>
    {
        let index;
        let entry_type;
        let name;
        let name_len: u16;
        self.reader.seek(SeekFrom::Start(offset))?;
        {
            let mut buf = [0; NAME_ENTRY_MIN_SIZE];
            self.reader.read_exact(&mut buf)?;
            index = LittleEndian::read_u32(&buf[0..4]);
            if index == 0 {
                bail!("Invalid entry index: 0");
            }
            entry_type = match LittleEndian::read_u32(&buf[4..8]) {
                0 => EntryType::File,
                1 => EntryType::Directory,
                v @ _ => bail!("Unknown entry type: 0x{:x}", v)
            };
            name_len = LittleEndian::read_u16(&buf[8..10]);
        }
        {
            let mut v = vec![0u8; name_len as usize];
            self.reader.read_exact(&mut v)?;
            name = String::from_utf8_lossy(&v).into_owned();
        }
        Ok(NameTableEntry {
            file_index: index,
            entry_type: entry_type,
            entry_size: NAME_ENTRY_MIN_SIZE as u32 + name_len as u32,
            name: name
        })
    }

    // FIXME: We might want to avoid recursive calls even if their number is limited
    fn read_directory_loop(&mut self, index: u32, stack: &mut Vec<u32>) -> Result<Directory>
    {
        let dentry = self.read_file_entry(index)?;
        let max_offset = dentry.offset as u64 + dentry.size as u64;
        let mut cur_offset = dentry.offset as u64;
        let mut dirs: Vec<Directory> = Vec::new();
        let mut files: Vec<File> = Vec::new();

        if stack.len() > 128 {
            bail!("Directory hierarchy is too deep (> 128 levels)");
        }
        if stack.contains(&index) {
            bail!("Directory loop detected for index 0x{:x}", index);
        }
        stack.push(index);

        while cur_offset < max_offset {
            let nentry = self.read_name_entry(cur_offset)?;
            let nentry_size = nentry.entry_size as u64; 
            if cur_offset + nentry_size > max_offset {
                bail!("Name entry at offset 0x{:x} spans outside of directory \
                       with index {}", cur_offset, index);
            }
            let fentry = self.read_file_entry(nentry.file_index)?;
            match nentry.entry_type {
                EntryType::File => {
                    files.push(File{
                        name_entry: nentry,
                        file_entry: fentry
                    });
                },
                EntryType::Directory => {
                    let undir = self.read_directory_loop(nentry.file_index, stack)?;
                    let dir = Directory {
                        files: undir.files,
                        directories: undir.directories,
                        file_entry: undir.file_entry,
                        name_entry: Some(nentry)
                    };
                    dirs.push(dir);
                }
            };
            cur_offset += nentry_size;
        }

        stack.pop();

        Ok(Directory {
            file_entry: dentry,
            name_entry: None,
            files: files,
            directories: dirs
        })
    }

    fn read_directory(&mut self, index: u32) -> Result<Directory>
    {
        let mut stack: Vec<u32> = Vec::new();
        return self.read_directory_loop(index, &mut stack);
    }

    fn read_rootdir(&mut self) -> Result<Directory>
    {
        self.read_directory(1)
    }

    fn open(filename: &str) -> Result<ArchiveFile> {
        let file = fs::File::open(filename)?;
        let basefile = file.try_clone()?;
        let mut filereader = BufReader::new(file);
        let filetbl_offset = ArchiveFile::read_header(&mut filereader)?;
        Ok(ArchiveFile {
            basefile: basefile,
            reader: filereader,
            filetbl_offset: filetbl_offset as u64
        })
    }
}

impl Archive {

    pub fn open(filename: &str) -> Result<Archive> {
        let mut file = ArchiveFile::open(filename)?;
        let rootdir = file.read_rootdir()?;
           Ok(Archive {
               file: file,
               rootdir: rootdir,
           })
    }

    pub fn file_data(&self, file: &File) -> Result<FileData>
    {
        let f = self.file.basefile.try_clone()?;
        FileData::new(f, &file.file_entry)
    }

    pub fn root_directory(&self) -> &Directory
    {
        &self.rootdir
    }

}



