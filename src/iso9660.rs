//! A minimal read-only ISO 9660 reader — just enough to list the files on a data CD and
//! find where each one lives. Prefers the Joliet tree (Unicode long names) when the disc
//! has one, falling back to plain ISO 9660 names.
//!
//! ISO 9660 stores each file as one contiguous run of 2048-byte sectors, so a file is fully
//! described by its byte offset and length.

use std::io;

pub const SECTOR: u64 = 2048;
/// Volume descriptors start here, after the system area.
const FIRST_DESCRIPTOR: u64 = 16;
/// Guards against corrupt or looping directory trees.
const MAX_DEPTH: usize = 8;
const MAX_DIR_BYTES: u32 = 4 << 20;

/// Random access to the disc (or a disc image in tests).
pub trait ReadAt {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAt for [u8] {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let src = self
            .get(start..start + buf.len())
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl ReadAt for Vec<u8> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.as_slice().read_exact_at(buf, offset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsoFile {
    /// Slash-separated path from the root, e.g. `Album/01 Song.mp3`.
    pub path: String,
    /// Byte offset of the file's data on the disc.
    pub start: u64,
    pub len: u64,
}

#[derive(Debug)]
pub struct Volume {
    pub label: String,
    /// The volume creation timestamp as recorded (`YYYYMMDDHHMMSScc`), for identifying the disc.
    pub created: String,
    pub files: Vec<IsoFile>,
}

struct Record {
    extent: u32,
    len: u32,
    is_dir: bool,
    name: Vec<u8>,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Parse the directory record at the start of `b`, returning it and its length.
fn parse_record(b: &[u8]) -> Option<(Record, usize)> {
    let len = *b.first()? as usize;
    if len < 34 || len > b.len() {
        return None;
    }
    let name_len = b[32] as usize;
    let name = b.get(33..33 + name_len)?.to_vec();
    Some((
        Record {
            extent: le32(&b[2..6]),
            len: le32(&b[10..14]),
            is_dir: b[25] & 0x02 != 0,
            name,
        },
        len,
    ))
}

fn decode_name(raw: &[u8], joliet: bool) -> String {
    let name = if joliet {
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        raw.iter().map(|&c| c as char).collect()
    };
    // Drop the ";1" file version, and the trailing dot ISO names get when there's no
    // extension.
    let name = name.split(';').next().unwrap_or_default();
    name.strip_suffix('.').unwrap_or(name).to_string()
}

fn trimmed(text: String) -> String {
    text.trim_end_matches([' ', '\0']).to_string()
}

pub fn read_volume(dev: &(impl ReadAt + ?Sized)) -> io::Result<Volume> {
    let mut primary = None;
    let mut joliet = None;
    let mut label = String::new();
    let mut created = String::new();
    for n in FIRST_DESCRIPTOR..FIRST_DESCRIPTOR + 16 {
        let mut d = [0u8; SECTOR as usize];
        dev.read_exact_at(&mut d, n * SECTOR)?;
        if &d[1..6] != b"CD001" {
            if n == FIRST_DESCRIPTOR {
                return Err(invalid("not an ISO 9660 disc"));
            }
            break;
        }
        match d[0] {
            1 => {
                primary = parse_record(&d[156..190]).map(|(r, _)| r);
                label = trimmed(decode_name(&d[40..72], false));
                created = trimmed(decode_name(&d[813..829], false));
            }
            // A supplementary descriptor whose escape sequence names UCS-2 is Joliet.
            2 if d[88] == b'%' && d[89] == b'/' && matches!(d[90], b'@' | b'C' | b'E') => {
                joliet = parse_record(&d[156..190]).map(|(r, _)| r);
                label = trimmed(decode_name(&d[40..72], true));
            }
            255 => break,
            _ => {}
        }
    }
    let (root, is_joliet) = match (joliet, primary) {
        (Some(root), _) => (root, true),
        (None, Some(root)) => (root, false),
        (None, None) => return Err(invalid("no root directory")),
    };
    let mut files = Vec::new();
    walk(dev, &root, "", is_joliet, 0, &mut files)?;
    Ok(Volume {
        label,
        created,
        files,
    })
}

fn walk(
    dev: &(impl ReadAt + ?Sized),
    dir: &Record,
    prefix: &str,
    joliet: bool,
    depth: usize,
    out: &mut Vec<IsoFile>,
) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let len = dir.len.min(MAX_DIR_BYTES) as usize;
    let mut buf = vec![0u8; len.div_ceil(SECTOR as usize) * SECTOR as usize];
    dev.read_exact_at(&mut buf, u64::from(dir.extent) * SECTOR)?;
    buf.truncate(len);

    let mut pos = 0;
    while pos < buf.len() {
        // Records never straddle sectors; a zero length byte pads out the rest of one.
        let Some((record, record_len)) = parse_record(&buf[pos..]) else {
            pos = (pos / SECTOR as usize + 1) * SECTOR as usize;
            continue;
        };
        pos += record_len;
        // "\0" and "\1" are the directory itself and its parent.
        if matches!(record.name.as_slice(), [0] | [1]) {
            continue;
        }
        let name = decode_name(&record.name, joliet);
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if record.is_dir {
            walk(dev, &record, &path, joliet, depth + 1, out)?;
        } else {
            out.push(IsoFile {
                path,
                start: u64::from(record.extent) * SECTOR,
                len: u64::from(record.len),
            });
        }
    }
    Ok(())
}
