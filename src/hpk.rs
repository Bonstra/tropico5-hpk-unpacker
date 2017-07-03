extern crate byteorder;

use ::errors::*;
use std::io;
use std::io::prelude::*;
use std::io::BufReader;
use std::io::SeekFrom;
use std::io::Bytes;
use std::io::Take;
use std::fs;
use self::byteorder::{ByteOrder, LittleEndian};

const FILE_ENTRY_SIZE: usize = 8;
const NAME_ENTRY_MIN_SIZE: usize = 10;

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

pub struct FileData {
    file: fs::File,
    size: u64
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

impl FileData {
    pub fn file(self) -> Take<fs::File>
    {
        self.file.take(self.size)
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
            let mut buf = [0; 0x20];
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
        let mut f = self.file.basefile.try_clone()?;
        f.seek(SeekFrom::Start(file.file_entry.offset as u64))?;
        return Ok(FileData {
            file: f,
            size: file.file_entry.size as u64
        });
    }

    pub fn root_directory(&self) -> &Directory
    {
        &self.rootdir
    }

}



