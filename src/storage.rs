//! Maps pieces onto files and performs positional disk I/O.

use crate::metainfo::{FileEntry, Metainfo};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct Storage {
    root: PathBuf,
    files: Vec<FileEntry>,
    handles: Vec<Mutex<Option<Arc<File>>>>,
    piece_length: u64,
    total: u64,
    closed: AtomicBool,
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Fill `buf` from `off`; bytes past end-of-file are zeroed.
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            f.read_at(&mut buf[done..], off + done as u64)?
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            match f.seek_read(&mut buf[done..], off + done as u64) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(38) => 0, // ERROR_HANDLE_EOF
                Err(e) => return Err(e),
            }
        };
        if n == 0 {
            buf[done..].fill(0);
            break;
        }
        done += n;
    }
    Ok(())
}

impl Storage {
    pub fn new(root: &Path, meta: &Metainfo) -> Storage {
        Storage {
            root: root.to_path_buf(),
            files: meta.files.clone(),
            handles: meta.files.iter().map(|_| Mutex::new(None)).collect(),
            piece_length: meta.piece_length,
            total: meta.total_length,
            closed: AtomicBool::new(false),
        }
    }

    fn full_path(&self, i: usize) -> PathBuf {
        self.root.join(&self.files[i].path)
    }

    fn handle(&self, i: usize, create: bool) -> io::Result<Option<Arc<File>>> {
        if self.closed.load(Ordering::Relaxed) {
            return Err(io::Error::other("storage closed"));
        }
        let mut h = self.handles[i].lock().unwrap();
        if let Some(f) = &*h {
            return Ok(Some(f.clone()));
        }
        let path = self.full_path(i);
        if !create && !path.exists() {
            return Ok(None);
        }
        if create {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
        }
        let f = Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(create)
                .truncate(false)
                .open(&path)?,
        );
        *h = Some(f.clone());
        Ok(Some(f))
    }

    /// Call `f(file_index, file_offset, range_in_buffer)` for every file segment of a byte range.
    fn for_segments(
        &self,
        offset: u64,
        len: u64,
        mut f: impl FnMut(usize, u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> io::Result<()> {
        let end = offset + len;
        if end > self.total {
            return Err(io::Error::other("range out of bounds"));
        }
        // First file whose end is beyond `offset`.
        let mut i = self
            .files
            .partition_point(|fe| fe.offset + fe.length <= offset);
        while i < self.files.len() {
            let fe = &self.files[i];
            if fe.offset >= end {
                break;
            }
            if fe.length > 0 {
                let s = offset.max(fe.offset);
                let e = end.min(fe.offset + fe.length);
                if s < e {
                    f(
                        i,
                        s - fe.offset,
                        (s - offset) as usize..(e - offset) as usize,
                    )?;
                }
            }
            i += 1;
        }
        Ok(())
    }

    pub fn write(&self, piece: usize, begin: u64, data: &[u8]) -> io::Result<()> {
        let off = piece as u64 * self.piece_length + begin;
        self.for_segments(off, data.len() as u64, |i, foff, r| {
            if self.files[i].pad {
                return Ok(());
            }
            let f = self.handle(i, true)?.unwrap();
            write_at(&f, &data[r], foff)
        })
    }

    pub fn read(&self, piece: usize, begin: u64, buf: &mut [u8]) -> io::Result<()> {
        let off = piece as u64 * self.piece_length + begin;
        self.for_segments(off, buf.len() as u64, |i, foff, r| {
            if self.files[i].pad {
                buf[r].fill(0);
                return Ok(());
            }
            match self.handle(i, false)? {
                Some(f) => read_at(&f, &mut buf[r], foff),
                None => {
                    buf[r].fill(0);
                    Ok(())
                }
            }
        })
    }

    pub fn hash_piece(&self, piece: usize, len: u64) -> io::Result<[u8; 20]> {
        let mut h = sha1_smol::Sha1::new();
        let mut buf = vec![0u8; (256 * 1024).min(len as usize)];
        let mut pos = 0u64;
        while pos < len {
            let n = (len - pos).min(buf.len() as u64) as usize;
            self.read(piece, pos, &mut buf[..n])?;
            h.update(&buf[..n]);
            pos += n as u64;
        }
        Ok(h.digest().bytes())
    }

    /// True if any non-padding file with data already exists on disk.
    pub fn any_file_exists(&self) -> bool {
        (0..self.files.len())
            .any(|i| !self.files[i].pad && self.files[i].length > 0 && self.full_path(i).exists())
    }

    /// Check that files backing completed pieces still exist (detects external deletion).
    pub fn files_present_for(&self, have: &[bool]) -> bool {
        for (i, fe) in self.files.iter().enumerate() {
            if fe.pad || fe.length == 0 {
                continue;
            }
            let first = (fe.offset / self.piece_length) as usize;
            let last = ((fe.offset + fe.length - 1) / self.piece_length) as usize;
            let any = have
                .get(first..=last.min(have.len().saturating_sub(1)))
                .is_some_and(|s| s.iter().any(|&h| h));
            if !any {
                continue;
            }
            match fs::metadata(self.full_path(i)) {
                Ok(m) if m.len() > 0 => {}
                _ => return false,
            }
        }
        true
    }

    /// Create zero-length files so the layout is complete once downloading finishes.
    /// Files with priority 0 (skipped) are left out.
    pub fn create_empty_files(&self, prio: &[u8]) {
        for (i, fe) in self.files.iter().enumerate() {
            if fe.length == 0 && !fe.pad && prio.get(i).copied().unwrap_or(1) > 0 {
                let _ = self.handle(i, true);
            }
        }
    }

    /// Release all file handles (so files can be moved or deleted).
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        for h in &self.handles {
            *h.lock().unwrap() = None;
        }
    }

    /// Delete the torrent's files and any directories left empty.
    pub fn delete_files(&self) {
        self.close();
        let mut dirs = Vec::new();
        for i in 0..self.files.len() {
            let p = self.full_path(i);
            let _ = fs::remove_file(&p);
            let mut d = p.parent();
            while let Some(dir) = d {
                if dir == self.root || !dir.starts_with(&self.root) {
                    break;
                }
                if !dirs.contains(&dir.to_path_buf()) {
                    dirs.push(dir.to_path_buf());
                }
                d = dir.parent();
            }
        }
        // Deepest first.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs {
            let _ = fs::remove_dir(d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{Value, dict};

    #[test]
    fn spans_files() {
        let info = dict(vec![
            (
                "files",
                Value::List(vec![
                    dict(vec![
                        ("length", Value::Int(3)),
                        ("path", Value::List(vec![Value::s("a")])),
                    ]),
                    dict(vec![
                        ("length", Value::Int(0)),
                        ("path", Value::List(vec![Value::s("empty")])),
                    ]),
                    dict(vec![
                        ("length", Value::Int(6)),
                        ("path", Value::List(vec![Value::s("d"), Value::s("b")])),
                    ]),
                ]),
            ),
            ("name", Value::s("t")),
            ("piece length", Value::Int(4)),
            ("pieces", Value::Bytes(vec![0; 60])),
        ]);
        let m = Metainfo::from_info(info.encode()).unwrap();
        let dir = std::env::temp_dir().join(format!("rt-storage-{}", crate::util::rand_u64()));
        let s = Storage::new(&dir, &m);
        s.write(0, 0, b"abcd").unwrap();
        s.write(1, 0, b"efgh").unwrap();
        s.write(2, 0, b"i").unwrap();
        s.create_empty_files(&[]);
        let mut b = [0u8; 4];
        s.read(0, 2, &mut b).unwrap();
        assert_eq!(&b, b"cdef");
        assert_eq!(fs::read(dir.join("t/a")).unwrap(), b"abc");
        assert_eq!(fs::read(dir.join("t/d/b")).unwrap(), b"defghi");
        assert!(dir.join("t/empty").exists());
        s.delete_files();
        assert!(!dir.join("t/d").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
