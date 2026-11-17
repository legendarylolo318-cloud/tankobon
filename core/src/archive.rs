//! Reading comic archives (CBZ/ZIP, CBR/RAR, CB7/7z).

use crate::util::natural_cmp;
use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Largest single page we accept from an archive. Protects against corrupt
/// headers and decompression bombs claiming gigabyte-sized entries.
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// Read at most [`MAX_ENTRY_BYTES`] from `r`; larger entries are an error.
fn read_bounded(r: &mut impl Read, size_hint: u64) -> Result<Vec<u8>> {
    if size_hint > MAX_ENTRY_BYTES {
        anyhow::bail!("archive entry too large ({size_hint} bytes)");
    }
    let mut buf = Vec::with_capacity(size_hint.min(16 * 1024 * 1024) as usize);
    r.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_ENTRY_BYTES {
        anyhow::bail!("archive entry too large");
    }
    Ok(buf)
}

pub const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "avif"];

pub fn is_image_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    if lower.rsplit('/').next().is_some_and(|f| f.starts_with('.')) {
        return false;
    }
    Path::new(&lower).extension().and_then(|e| e.to_str()).is_some_and(|e| IMAGE_EXTS.contains(&e))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Rar,
    SevenZ,
}

pub fn archive_kind(path: &Path) -> Option<ArchiveKind> {
    match path.extension()?.to_str()?.to_lowercase().as_str() {
        "cbz" | "zip" => Some(ArchiveKind::Zip),
        "cbr" | "rar" => Some(ArchiveKind::Rar),
        "cb7" | "7z" => Some(ArchiveKind::SevenZ),
        _ => None,
    }
}

pub fn is_archive(path: &Path) -> bool {
    archive_kind(path).is_some()
}

pub struct ArchiveReader {
    path: PathBuf,
    kind: ArchiveKind,
}

impl ArchiveReader {
    pub fn open(path: &Path) -> Result<Self> {
        let kind = archive_kind(path).ok_or_else(|| anyhow!("not a supported archive: {}", path.display()))?;
        Ok(Self { path: path.to_path_buf(), kind })
    }

    pub fn kind(&self) -> ArchiveKind {
        self.kind
    }

    /// Image entry names in natural order.
    pub fn list_images(&self) -> Result<Vec<String>> {
        let mut names = match self.kind {
            ArchiveKind::Zip => {
                let zip = zip::ZipArchive::new(File::open(&self.path)?)?;
                zip.file_names().filter_map(|n| n.ok()).filter(|n| !n.ends_with('/')).map(|n| n.into_owned()).collect::<Vec<_>>()
            }
            ArchiveKind::Rar => {
                let ar = unrar::Archive::new(&self.path).open_for_listing().map_err(|e| anyhow!("rar: {e}"))?;
                let mut v = Vec::new();
                for e in ar {
                    let e = e.map_err(|e| anyhow!("rar: {e}"))?;
                    if e.is_file() {
                        v.push(e.filename.to_string_lossy().replace('\\', "/"));
                    }
                }
                v
            }
            ArchiveKind::SevenZ => {
                let r = sevenz_rust2::ArchiveReader::new(File::open(&self.path)?, sevenz_rust2::Password::empty())
                    .map_err(|e| anyhow!("7z: {e}"))?;
                r.archive().files.iter().filter(|f| !f.is_directory()).map(|f| f.name().to_string()).collect()
            }
        };
        names.retain(|n| is_image_name(n));
        names.sort_by(|a, b| natural_cmp(a, b));
        Ok(names)
    }

    /// Read a single entry. Cheap for ZIP; RAR/7z may need to scan, so prefer `extract_all` for those.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        match self.kind {
            ArchiveKind::Zip => {
                let mut zip = zip::ZipArchive::new(File::open(&self.path)?)?;
                let mut f = zip.by_name(name).with_context(|| format!("entry {name}"))?;
                let size = f.size();
                read_bounded(&mut f, size)
            }
            ArchiveKind::Rar => {
                let mut ar = unrar::Archive::new(&self.path).open_for_processing().map_err(|e| anyhow!("rar: {e}"))?;
                while let Some(header) = ar.read_header().map_err(|e| anyhow!("rar: {e}"))? {
                    if header.entry().filename.to_string_lossy().replace('\\', "/") == name {
                        if header.entry().unpacked_size > MAX_ENTRY_BYTES {
                            anyhow::bail!("archive entry too large");
                        }
                        let (data, _) = header.read().map_err(|e| anyhow!("rar: {e}"))?;
                        return Ok(data);
                    }
                    ar = header.skip().map_err(|e| anyhow!("rar: {e}"))?;
                }
                Err(anyhow!("entry {name} not found"))
            }
            ArchiveKind::SevenZ => {
                let mut r = sevenz_rust2::ArchiveReader::new(File::open(&self.path)?, sevenz_rust2::Password::empty())
                    .map_err(|e| anyhow!("7z: {e}"))?;
                if r.archive().files.iter().any(|f| f.name() == name && f.size() > MAX_ENTRY_BYTES) {
                    anyhow::bail!("archive entry too large");
                }
                r.read_file(name).map_err(|e| anyhow!("7z: {e}"))
            }
        }
    }

    /// Extract all images into `dest` as `00001.ext`, `00002.ext`... in natural order.
    pub fn extract_all(&self, dest: &Path) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dest)?;
        let names = self.list_images()?;
        let mut out = Vec::with_capacity(names.len());
        match self.kind {
            ArchiveKind::Zip => {
                for (i, n) in names.iter().enumerate() {
                    let p = dest.join(numbered(i, n));
                    std::fs::write(&p, self.read(n)?)?;
                    out.push(p);
                }
            }
            ArchiveKind::Rar => {
                let index: std::collections::HashMap<&str, usize> = names.iter().enumerate().map(|(i, n)| (n.as_str(), i)).collect();
                let mut ar = unrar::Archive::new(&self.path).open_for_processing().map_err(|e| anyhow!("rar: {e}"))?;
                let mut paths: Vec<Option<PathBuf>> = vec![None; names.len()];
                while let Some(header) = ar.read_header().map_err(|e| anyhow!("rar: {e}"))? {
                    let n = header.entry().filename.to_string_lossy().replace('\\', "/");
                    if let Some(&i) = index.get(n.as_str()) {
                        if header.entry().unpacked_size > MAX_ENTRY_BYTES {
                            anyhow::bail!("archive entry too large");
                        }
                        let (data, next) = header.read().map_err(|e| anyhow!("rar: {e}"))?;
                        let p = dest.join(numbered(i, &n));
                        std::fs::write(&p, data)?;
                        paths[i] = Some(p);
                        ar = next;
                    } else {
                        ar = header.skip().map_err(|e| anyhow!("rar: {e}"))?;
                    }
                }
                out = paths.into_iter().flatten().collect();
            }
            ArchiveKind::SevenZ => {
                let mut r = sevenz_rust2::ArchiveReader::new(File::open(&self.path)?, sevenz_rust2::Password::empty())
                    .map_err(|e| anyhow!("7z: {e}"))?;
                let index: std::collections::HashMap<String, usize> = names.iter().cloned().enumerate().map(|(i, n)| (n, i)).collect();
                let mut paths: Vec<Option<PathBuf>> = vec![None; names.len()];
                r.for_each_entries(|entry, reader| {
                    if let Some(&i) = index.get(entry.name()) {
                        let mut buf = Vec::new();
                        reader.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut buf)?;
                        if buf.len() as u64 > MAX_ENTRY_BYTES {
                            return Err(std::io::Error::other("archive entry too large").into());
                        }
                        let p = dest.join(numbered(i, entry.name()));
                        std::fs::write(&p, buf)?;
                        paths[i] = Some(p);
                    }
                    Ok(true)
                })
                .map_err(|e| anyhow!("7z: {e}"))?;
                out = paths.into_iter().flatten().collect();
            }
        }
        Ok(out)
    }
}

fn numbered(i: usize, name: &str) -> String {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("jpg").to_lowercase();
    format!("{:05}.{ext}", i + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        for (n, d) in entries {
            w.start_file(*n, opts).unwrap();
            w.write_all(d).unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn image_name_detection() {
        assert!(is_image_name("a/b/001.JPG"));
        assert!(is_image_name("x.webp"));
        assert!(!is_image_name("ComicInfo.xml"));
        assert!(!is_image_name("__MACOSX/._001.jpg"));
        assert!(!is_image_name("thumbs.db"));
    }

    #[test]
    fn archive_kinds() {
        assert_eq!(archive_kind(Path::new("a.CBZ")), Some(ArchiveKind::Zip));
        assert_eq!(archive_kind(Path::new("a.cbr")), Some(ArchiveKind::Rar));
        assert_eq!(archive_kind(Path::new("a.cb7")), Some(ArchiveKind::SevenZ));
        assert_eq!(archive_kind(Path::new("a.pdf")), None);
    }

    #[test]
    fn zip_lists_sorted_images_and_reads() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.cbz");
        make_zip(&p, &[("10.png", b"ten"), ("2.png", b"two"), ("1.png", b"one"), ("info.xml", b"x"), ("sub/", b"")]);
        let r = ArchiveReader::open(&p).unwrap();
        assert_eq!(r.list_images().unwrap(), vec!["1.png", "2.png", "10.png"]);
        assert_eq!(r.read("2.png").unwrap(), b"two");
        assert!(r.read("missing.png").is_err());
    }

    #[test]
    fn zip_extract_all_orders_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("book.zip");
        make_zip(&p, &[("b.jpg", b"B"), ("a.jpg", b"A")]);
        let out = ArchiveReader::open(&p).unwrap().extract_all(&dir.path().join("out")).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(std::fs::read(&out[0]).unwrap(), b"A");
        assert_eq!(std::fs::read(&out[1]).unwrap(), b"B");
    }

    #[test]
    fn sevenz_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("2.png"), b"two").unwrap();
        std::fs::write(src.join("1.png"), b"one").unwrap();
        let p = dir.path().join("book.cb7");
        sevenz_rust2::compress_to_path(&src, &p).unwrap();
        let r = ArchiveReader::open(&p).unwrap();
        let names = r.list_images().unwrap();
        assert_eq!(names.len(), 2);
        assert!(names[0].ends_with("1.png"));
        assert_eq!(r.read(&names[1]).unwrap(), b"two");
        let out = r.extract_all(&dir.path().join("out")).unwrap();
        assert_eq!(std::fs::read(&out[0]).unwrap(), b"one");
    }

    #[test]
    fn malformed_archives_error_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["bad.cbz", "bad.cbr", "bad.cb7"] {
            let p = dir.path().join(name);
            std::fs::write(&p, b"PK\x03\x04 this is not really an archive").unwrap();
            let r = ArchiveReader::open(&p).unwrap();
            assert!(r.list_images().is_err(), "{name} list");
            assert!(r.read("1.png").is_err(), "{name} read");
            assert!(r.extract_all(&dir.path().join(format!("out-{name}"))).is_err(), "{name} extract");
        }
        // a truncated but otherwise valid zip
        let p = dir.path().join("trunc.cbz");
        make_zip(&p, &[("1.png", &[7u8; 4096])]);
        let bytes = std::fs::read(&p).unwrap();
        std::fs::write(&p, &bytes[..bytes.len() / 2]).unwrap();
        assert!(ArchiveReader::open(&p).unwrap().list_images().is_err());
    }

    #[test]
    fn bounded_reads_reject_oversized_entries() {
        let mut big = std::io::repeat(0u8);
        assert!(read_bounded(&mut big, MAX_ENTRY_BYTES + 1).is_err(), "size header too large");
        let mut small = std::io::Cursor::new(vec![1u8; 10]);
        assert_eq!(read_bounded(&mut small, 10).unwrap().len(), 10);
    }

    #[test]
    fn rejects_unknown_extension() {
        assert!(ArchiveReader::open(Path::new("x.txt")).is_err());
    }
}
