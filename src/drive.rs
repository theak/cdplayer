//! The optical drive, driven by Linux cdrom and SG_IO ioctls on its block device — no
//! udev, `eject`, or `sg_raw` needed: wake an Apple SuperDrive, check for an audio disc,
//! read its track list, read raw audio, and eject.
//!
//! Every call opens the device with `O_NONBLOCK`, which skips the kernel's media check and
//! door lock, so polling never interferes with a disc being inserted.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

// <linux/cdrom.h>
const CDROMREADTOCHDR: u32 = 0x5305;
const CDROMREADTOCENTRY: u32 = 0x5306;
const CDROMEJECT: u32 = 0x5309;
const CDROM_DRIVE_STATUS: u32 = 0x5326;
const CDROM_DISC_STATUS: u32 = 0x5327;
const CDROM_LOCKDOOR: u32 = 0x5329;
const CDSL_CURRENT: libc::c_ulong = i32::MAX as libc::c_ulong;
const CDS_DISC_OK: i32 = 4;
const CDS_AUDIO: i32 = 100;
const CDS_MIXED: i32 = 105;
const CDROM_LBA: u8 = 0x01;
const CDROM_LEADOUT: u8 = 0xAA;
const CDROM_DATA_TRACK: u8 = 0x04;

// <scsi/sg.h>
const SG_IO: u32 = 0x2285;
const SG_DXFER_NONE: libc::c_int = -1;
const SG_DXFER_FROM_DEV: libc::c_int = -3;

/// One CD sector of audio: 588 stereo 16-bit little-endian samples at 44.1 kHz.
pub const SECTOR_BYTES: usize = 2352;
pub const SECTORS_PER_SECOND: u32 = 75;

/// Apple's vendor command that switches a SuperDrive on. Macs send it automatically; on
/// anything else the drive stays asleep (won't take a disc) until it's sent.
const SUPERDRIVE_WAKE: [u8; 7] = [0xEA, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];

/// Gap between sessions on an Enhanced CD (audio session, then a data session): the first
/// session's lead-out plus the second's lead-in, which isn't readable audio.
const SESSION_GAP: u32 = 11_400;

/// What's in the drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disc {
    /// The drive isn't connected (or the device can't be opened).
    Missing,
    /// No disc, or the drive is still spinning one up.
    Empty,
    /// A disc with no audio tracks.
    Data,
    /// An audio (or mixed-mode) CD.
    Audio,
}

impl Disc {
    pub fn is_audio(self) -> bool {
        self == Disc::Audio
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Disc::Missing => "missing",
            Disc::Empty => "empty",
            Disc::Data => "data",
            Disc::Audio => "audio",
        }
    }
}

/// An audio track's sectors, `start..end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Track {
    pub start: u32,
    pub end: u32,
}

/// The audio tracks described by TOC entries — `(start sector, is data)` per track, in
/// order, ending with the lead-out.
pub fn audio_tracks(entries: &[(u32, bool)]) -> Vec<Track> {
    entries
        .windows(2)
        .filter(|w| !w[0].1)
        .map(|w| {
            let (start, _) = w[0];
            let (next, next_is_data) = w[1];
            let end = if next_is_data {
                next.saturating_sub(SESSION_GAP).max(start)
            } else {
                next
            };
            Track { start, end }
        })
        .collect()
}

/// `sg_io_hdr_t` from <scsi/sg.h>.
#[repr(C)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: libc::c_uchar,
    mx_sb_len: libc::c_uchar,
    iovec_count: libc::c_ushort,
    dxfer_len: libc::c_uint,
    dxferp: *mut libc::c_void,
    cmdp: *const libc::c_uchar,
    sbp: *mut libc::c_uchar,
    timeout: libc::c_uint,
    flags: libc::c_uint,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: libc::c_uchar,
    masked_status: libc::c_uchar,
    msg_status: libc::c_uchar,
    sb_len_wr: libc::c_uchar,
    host_status: libc::c_ushort,
    driver_status: libc::c_ushort,
    resid: libc::c_int,
    duration: libc::c_uint,
    info: libc::c_uint,
}

/// `struct cdrom_tochdr` from <linux/cdrom.h>.
#[repr(C)]
struct TocHeader {
    first_track: u8,
    last_track: u8,
}

/// `struct cdrom_tocentry` from <linux/cdrom.h>, with the address read as an LBA.
#[repr(C)]
struct TocEntry {
    track: u8,
    /// `cdte_adr:4, cdte_ctrl:4` bitfields — ctrl is the high nibble.
    adr_ctrl: u8,
    format: u8,
    lba: libc::c_int,
    datamode: u8,
}

fn ioctl(f: &File, request: u32, arg: libc::c_ulong) -> io::Result<i32> {
    // SAFETY: `f` is an open fd; callers pass either a plain integer or a pointer to a
    // live, correctly laid-out struct for `request`.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), request as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// Send a SCSI command, reading its response (if any) into `data`.
fn scsi(f: &File, cdb: &[u8], data: &mut [u8]) -> io::Result<()> {
    let mut sense = [0u8; 32];
    let mut hdr = SgIoHdr {
        interface_id: b'S' as libc::c_int,
        dxfer_direction: if data.is_empty() { SG_DXFER_NONE } else { SG_DXFER_FROM_DEV },
        cmd_len: cdb.len() as u8,
        mx_sb_len: sense.len() as u8,
        iovec_count: 0,
        dxfer_len: data.len() as libc::c_uint,
        dxferp: data.as_mut_ptr().cast(),
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: 10_000,
        flags: 0,
        pack_id: 0,
        usr_ptr: std::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };
    ioctl(f, SG_IO, &mut hdr as *mut SgIoHdr as libc::c_ulong)?;
    if hdr.status != 0 || hdr.host_status != 0 || hdr.driver_status != 0 {
        return Err(io::Error::other(format!(
            "SCSI command {:#04x} failed (status {:#x})",
            cdb[0], hdr.status
        )));
    }
    Ok(())
}

pub struct Drive {
    path: String,
}

impl Drive {
    pub fn new(path: impl Into<String>) -> Self {
        Drive { path: path.into() }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    fn open(&self, write: bool) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.path)
    }

    /// Send the SuperDrive wake command. Other drives reject it, which is harmless.
    pub fn wake(&self) -> io::Result<()> {
        // A vendor-specific command is only allowed through SG_IO on a writable open.
        scsi(&self.open(true)?, &SUPERDRIVE_WAKE, &mut [])
    }

    /// Whether a disc is in, without touching it (no TOC read). `None` if the drive is gone.
    pub fn has_disc(&self) -> Option<bool> {
        let f = self.open(false).ok()?;
        ioctl(&f, CDROM_DRIVE_STATUS, CDSL_CURRENT)
            .ok()
            .map(|s| s == CDS_DISC_OK)
    }

    pub fn status(&self) -> Disc {
        let Ok(f) = self.open(false) else {
            return Disc::Missing;
        };
        match ioctl(&f, CDROM_DRIVE_STATUS, CDSL_CURRENT) {
            Ok(CDS_DISC_OK) => {}
            Ok(_) => return Disc::Empty,
            Err(_) => return Disc::Missing,
        }
        match ioctl(&f, CDROM_DISC_STATUS, 0) {
            Ok(CDS_AUDIO | CDS_MIXED) => Disc::Audio,
            _ => Disc::Data,
        }
    }

    /// The disc's audio tracks.
    pub fn tracks(&self) -> io::Result<Vec<Track>> {
        let f = self.open(false)?;
        let mut header = TocHeader {
            first_track: 0,
            last_track: 0,
        };
        ioctl(&f, CDROMREADTOCHDR, &mut header as *mut TocHeader as libc::c_ulong)?;
        let mut entries = Vec::new();
        for track in (header.first_track..=header.last_track).chain([CDROM_LEADOUT]) {
            let mut entry = TocEntry {
                track,
                adr_ctrl: 0,
                format: CDROM_LBA,
                lba: 0,
                datamode: 0,
            };
            ioctl(&f, CDROMREADTOCENTRY, &mut entry as *mut TocEntry as libc::c_ulong)?;
            let is_data = track != CDROM_LEADOUT && (entry.adr_ctrl >> 4) & CDROM_DATA_TRACK != 0;
            entries.push((entry.lba.max(0) as u32, is_data));
        }
        Ok(audio_tracks(&entries))
    }

    pub fn audio_reader(&self) -> io::Result<AudioReader> {
        Ok(AudioReader(self.open(false)?))
    }

    /// Unlock the door and eject. Fails with EBUSY if anything else still has the device
    /// open (normally), so stop playback first.
    pub fn eject(&self) -> io::Result<()> {
        let f = self.open(false)?;
        let _ = ioctl(&f, CDROM_LOCKDOOR, 0);
        ioctl(&f, CDROMEJECT, 0)?;
        Ok(())
    }
}

/// An open handle for reading raw audio sectors.
pub struct AudioReader(File);

impl AudioReader {
    /// Fill `buf` (a whole number of sectors) with raw audio starting at sector `lba`,
    /// using MMC READ CD.
    pub fn read(&self, lba: u32, buf: &mut [u8]) -> io::Result<()> {
        let sectors = (buf.len() / SECTOR_BYTES) as u32;
        let [_, len_hi, len_mid, len_lo] = sectors.to_be_bytes();
        let [lba0, lba1, lba2, lba3] = lba.to_be_bytes();
        let cdb = [
            0xBE, // READ CD
            0x04, // expected sector type: CD-DA
            lba0, lba1, lba2, lba3, len_hi, len_mid, len_lo,
            0x10, // return user data (the audio samples) only
            0x00, // no subchannel data
            0x00,
        ];
        scsi(&self.0, &cdb, buf)
    }
}
