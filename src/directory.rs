use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read, Write};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub inode: u64,
    pub hash: u64,
    pub name: String,
}

#[derive(Error, Debug)]
pub enum DirectoryError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Encoding error")]
    Utf8Error(#[from] std::string::FromUtf8Error),
    #[error("Entry too large")]
    EntryTooLarge,
}

impl DirectoryEntry {
    pub const MAX_FILENAME_LEN: usize = 255;

    pub fn serialize_into<W: Write>(&self, writer: &mut W) -> Result<(), DirectoryError> {
        if self.name.contains('/') {
            return Err(DirectoryError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Filename cannot contain '/'")));
        }
        let name_bytes = self.name.as_bytes();
        if name_bytes.len() > Self::MAX_FILENAME_LEN {
            return Err(DirectoryError::EntryTooLarge);
        }
        
        writer.write_all(&self.inode.to_le_bytes())?;
        writer.write_all(&self.hash.to_le_bytes())?;
        writer.write_all(&(name_bytes.len() as u16).to_le_bytes())?;
        writer.write_all(name_bytes)?;
        
        Ok(())
    }

    pub fn deserialize_from<R: Read>(reader: &mut R) -> Result<Option<Self>, DirectoryError> {
        let mut inode_buf = [0u8; 8];
        if reader.read_exact(&mut inode_buf).is_err() {
            return Ok(None);
        }
        let inode = u64::from_le_bytes(inode_buf);
        
        let mut hash_buf = [0u8; 8];
        if reader.read_exact(&mut hash_buf).is_err() { return Ok(None); }
        let hash = u64::from_le_bytes(hash_buf);

        let mut len_buf = [0u8; 2];
        if reader.read_exact(&mut len_buf).is_err() { return Ok(None); }
        let len = u16::from_le_bytes(len_buf) as usize;

        if len == 0 {
            // Assume 0 length name means end of entries
            return Ok(None);
        }

        let mut name_buf = vec![0u8; len];
        reader.read_exact(&mut name_buf)?;
        let name = String::from_utf8(name_buf)?;
        // If len == 0 -> End.
        
        // Wait, I cannot read `name` before `len`.
        // So correct flow:
        // read inode, hash, len.
        // if len == 0 -> return None (End).
        // else read name.
        
        Ok(Some(Self {
            inode,
            hash,
            name,
        }))
    }
}

pub struct DirectoryIterator<'a> {
    cursor: Cursor<&'a [u8]>,
}

impl<'a> DirectoryIterator<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            cursor: Cursor::new(data),
        }
    }
}

impl<'a> Iterator for DirectoryIterator<'a> {
    type Item = Result<DirectoryEntry, DirectoryError>;

    fn next(&mut self) -> Option<Self::Item> {
        // If we are at end of position?
        if self.cursor.position() >= self.cursor.get_ref().len() as u64 {
            return None;
        }

        match DirectoryEntry::deserialize_from(&mut self.cursor) {
            Ok(Some(entry)) => Some(Ok(entry)),
            Ok(None) => None, // End of entries
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_directory_entry_serialization_roundtrip() {
        let entry = DirectoryEntry {
            inode: 42,
            hash: 1337,
            name: "hello_world.txt".to_string(),
        };

        let mut buf = Vec::new();
        entry.serialize_into(&mut buf).expect("serialize");

        let mut cursor = Cursor::new(&buf);
        let deserialized = DirectoryEntry::deserialize_from(&mut cursor)
            .expect("deserialize")
            .expect("some entry");

        assert_eq!(deserialized, entry);
    }

    #[test]
    fn test_directory_entry_reject_slash_in_name() {
        let entry = DirectoryEntry {
            inode: 1,
            hash: 0,
            name: "invalid/path.txt".to_string(),
        };

        let mut buf = Vec::new();
        let err = entry.serialize_into(&mut buf).unwrap_err();
        assert!(matches!(err, DirectoryError::Io(_)));
    }

    #[test]
    fn test_directory_entry_max_name_limit() {
        let long_name = "a".repeat(256);
        let entry = DirectoryEntry {
            inode: 1,
            hash: 0,
            name: long_name,
        };

        let mut buf = Vec::new();
        let err = entry.serialize_into(&mut buf).unwrap_err();
        assert!(matches!(err, DirectoryError::EntryTooLarge));
    }

    #[test]
    fn test_directory_iterator_multiple_entries() {
        let entries = vec![
            DirectoryEntry { inode: 1, hash: 0, name: "file1.txt".to_string() },
            DirectoryEntry { inode: 2, hash: 0, name: "file2.txt".to_string() },
            DirectoryEntry { inode: 3, hash: 0, name: "dir1".to_string() },
        ];

        let mut buf = vec![0u8; 4096];
        let mut cursor = Cursor::new(&mut buf[..]);
        for entry in &entries {
            entry.serialize_into(&mut cursor).expect("serialize");
        }

        let iter = DirectoryIterator::new(&buf);
        let collected: Result<Vec<_>, _> = iter.collect();
        let collected = collected.expect("collect entries");

        assert_eq!(collected, entries);
    }

    #[test]
    fn test_directory_corrupt_utf8_name() {
        // Construct invalid UTF-8 entry manually
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u64.to_le_bytes()); // inode
        buf.extend_from_slice(&0u64.to_le_bytes()); // hash
        buf.extend_from_slice(&2u16.to_le_bytes()); // len = 2
        buf.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8 bytes

        let mut cursor = Cursor::new(&buf);
        let err = DirectoryEntry::deserialize_from(&mut cursor).unwrap_err();
        assert!(matches!(err, DirectoryError::Utf8Error(_)));
    }
}
