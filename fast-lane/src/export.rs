//! Bounded, rotating line-oriented export files (per-transaction CSV, mismatch samples,
//! interval summaries). Written only by the comparator thread.

use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

pub struct RotatingWriter {
    dir: PathBuf,
    prefix: String,
    ext: String,
    header: Option<String>,
    max_bytes: u64,
    max_files: usize,
    seq: u64,
    bytes: u64,
    file: Option<BufWriter<File>>,
    pub errors: u64,
}

impl RotatingWriter {
    pub fn new(
        dir: &Path,
        prefix: &str,
        ext: &str,
        header: Option<&str>,
        max_bytes: u64,
        max_files: usize,
    ) -> std::io::Result<Self> {
        fs::create_dir_all(dir)?;
        // Continue numbering after existing files so restarts never overwrite.
        let mut seq = 0;
        if let Ok(read_dir) = fs::read_dir(dir) {
            for entry in read_dir.flatten() {
                if let Some(n) = parse_seq(&entry.file_name().to_string_lossy(), prefix, ext) {
                    seq = seq.max(n + 1);
                }
            }
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            prefix: prefix.to_string(),
            ext: ext.to_string(),
            header: header.map(str::to_string),
            max_bytes: max_bytes.max(1 << 16),
            max_files: max_files.max(1),
            seq,
            bytes: 0,
            file: None,
            errors: 0,
        })
    }

    fn path_for(&self, seq: u64) -> PathBuf {
        self.dir
            .join(format!("{}.{seq:06}.{}", self.prefix, self.ext))
    }

    fn open_next(&mut self) -> std::io::Result<()> {
        if let Some(mut f) = self.file.take() {
            f.flush()?;
        }
        let path = self.path_for(self.seq);
        self.seq += 1;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        self.bytes = 0;
        if let Some(header) = &self.header {
            writer.write_all(header.as_bytes())?;
            writer.write_all(b"\n")?;
            self.bytes += header.len() as u64 + 1;
        }
        self.file = Some(writer);
        // Delete the oldest files beyond the cap.
        if self.seq > self.max_files as u64 {
            let oldest_kept = self.seq - self.max_files as u64;
            if let Ok(read_dir) = fs::read_dir(&self.dir) {
                for entry in read_dir.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if let Some(n) = parse_seq(&name, &self.prefix, &self.ext) {
                        if n < oldest_kept {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn write_line(&mut self, line: &str) {
        if self.file.is_none() || self.bytes >= self.max_bytes {
            if self.open_next().is_err() {
                self.errors += 1;
                return;
            }
        }
        if let Some(f) = self.file.as_mut() {
            if f.write_all(line.as_bytes()).and_then(|_| f.write_all(b"\n")).is_err() {
                self.errors += 1;
                return;
            }
            self.bytes += line.len() as u64 + 1;
        }
    }

    pub fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            if f.flush().is_err() {
                self.errors += 1;
            }
        }
    }
}

fn parse_seq(name: &str, prefix: &str, ext: &str) -> Option<u64> {
    name.strip_prefix(prefix)?
        .strip_prefix('.')?
        .strip_suffix(ext)?
        .strip_suffix('.')?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = RotatingWriter::new(dir.path(), "fl_tx", "csv", Some("h"), 1 << 16, 3).unwrap();
        let line = "x".repeat(1000);
        for _ in 0..400 {
            w.write_line(&line);
        }
        w.flush();
        let mut files: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert_eq!(files.len(), 3, "{files:?}");
        assert!(files[0].starts_with("fl_tx.") && files[0].ends_with(".csv"));
        // A new writer continues numbering.
        let w2 = RotatingWriter::new(dir.path(), "fl_tx", "csv", None, 1 << 16, 3).unwrap();
        assert!(w2.seq >= 6);
    }
}
