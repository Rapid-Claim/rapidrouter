//! Write-ahead log: one JSON line per committed command, CRC-protected,
//! fsynced per append (config writes are rare — durability wins).
//!
//! Recovery is torn-write tolerant: replay stops at the first line that
//! fails to parse or checksum, and the file is truncated back to the last
//! good entry — the standard contract for a crash mid-append.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::Command;

pub struct Wal {
    path: PathBuf,
    file: File,
}

pub struct Entry {
    pub index: u64,
    pub command: Command,
}

#[derive(Serialize, Deserialize)]
struct Line {
    i: u64,
    crc: u32,
    c: serde_json::Value,
}

impl Wal {
    /// Open the log, replaying every valid entry and truncating any
    /// corrupt tail.
    pub fn open(path: &Path) -> std::io::Result<(Self, Vec<Entry>)> {
        let mut entries = Vec::new();
        let mut good_bytes: u64 = 0;
        let mut truncated = false;

        if let Ok(file) = File::open(path) {
            let mut reader = BufReader::new(file);
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line)?;
                if n == 0 {
                    break;
                }
                match parse_line(&line) {
                    Some(entry) => {
                        good_bytes += n as u64;
                        entries.push(entry);
                    }
                    None => {
                        truncated = true;
                        break;
                    }
                }
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false) // recovery already decided what to keep
            .read(true)
            .write(true)
            .open(path)?;
        if truncated {
            file.set_len(good_bytes)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                path: path.to_owned(),
                file,
            },
            entries,
        ))
    }

    pub fn append(&mut self, index: u64, command: &Command) -> std::io::Result<()> {
        let payload = serde_json::to_value(command).expect("commands serialize");
        let crc = crc32fast::hash(payload.to_string().as_bytes());
        let mut line = serde_json::to_string(&Line {
            i: index,
            crc,
            c: payload,
        })
        .expect("wal lines serialize");
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.sync_all()?;
        Ok(())
    }

    /// Empty the log after its contents were folded into a snapshot.
    pub fn truncate(&mut self) -> std::io::Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        // Re-open path kept for diagnostics; nothing else to do.
        let _ = &self.path;
        Ok(())
    }
}

fn parse_line(line: &str) -> Option<Entry> {
    let trimmed = line.trim_end_matches('\n');
    if trimmed.is_empty() {
        return None;
    }
    let parsed: Line = serde_json::from_str(trimmed).ok()?;
    if crc32fast::hash(parsed.c.to_string().as_bytes()) != parsed.crc {
        return None;
    }
    let command: Command = serde_json::from_value(parsed.c).ok()?;
    Some(Entry {
        index: parsed.i,
        command,
    })
}
