//! The drive's files, under one directory and never outside it. A path is
//! cleaned to its components (relative, no `.`, `..`, empty or hidden
//! ones), then walked from the root a component at a time with
//! `O_NOFOLLOW`, so a symbolic link anywhere on the way, put there by
//! anyone, fails the request instead of leading out. Only regular files and
//! directories are served; a write goes to a hidden temporary file beside
//! its name, renamed into place once whole.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

/// How deep a path may go.
const MAX_DEPTH: usize = 32;
/// Entries a listing has, at most.
pub const MAX_ENTRIES: usize = 2000;

fn errno() -> io::Error {
    io::Error::last_os_error()
}

fn bad(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, why.into())
}

/// A path's components, as the drive takes them: percent-decoded,
/// relative, without `.`, `..`, empty or hidden (`.`-prefixed) ones, each
/// printable. The office's drive cleans archive paths the same way.
pub fn components(path: &str) -> io::Result<Vec<String>> {
    let decoded = percent_decode(path).ok_or_else(|| bad("a path that isn't UTF-8"))?;
    let parts: Vec<String> = decoded
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .map(str::to_string)
        .collect();
    if parts.len() > MAX_DEPTH {
        return Err(bad(format!("a path at most {MAX_DEPTH} deep")));
    }
    for p in &parts {
        if p == ".." || p.starts_with('.') || p.len() > 255 || p.chars().any(|c| c.is_control()) {
            return Err(bad(format!("{p:?} isn't a name the drive takes")));
        }
    }
    Ok(parts)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| bad("a name with a NUL"))
}

fn open_at(
    dir: &OwnedFd,
    name: &str,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<OwnedFd> {
    let n = cstr(name)?;
    let fd = unsafe { libc::openat(dir.as_raw_fd(), n.as_ptr(), flags | libc::O_CLOEXEC, mode) };
    if fd < 0 {
        return Err(errno());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
        return Err(errno());
    }
    Ok(st)
}

fn stat_at(dir: &OwnedFd, name: &str) -> io::Result<libc::stat> {
    let n = cstr(name)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            n.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } < 0
    {
        return Err(errno());
    }
    Ok(st)
}

fn is(st: &libc::stat, kind: libc::mode_t) -> bool {
    st.st_mode & libc::S_IFMT == kind
}

/// The drive: its root directory.
#[derive(Debug, Clone)]
pub struct Files {
    root: std::path::PathBuf,
}

/// What a path is.
pub enum Found {
    File { file: File, size: u64 },
    Dir(Vec<Entry>),
}

/// An entry of a listing.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Entry {
    pub name: String,
    /// `file` or `dir`.
    pub kind: &'static str,
    pub size: u64,
    /// Seconds since the epoch.
    pub modified: i64,
}

impl Files {
    pub fn new(root: &Path) -> Files {
        Files {
            root: root.to_path_buf(),
        }
    }

    fn root_fd(&self) -> io::Result<OwnedFd> {
        let p = CString::new(self.root.as_os_str().as_encoded_bytes()).map_err(|_| bad("root"))?;
        let fd = unsafe {
            libc::open(
                p.as_ptr(),
                libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(errno());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The directory `dirs` names, walked from the root without following a
    /// link; made as it goes if `create`.
    fn walk(&self, dirs: &[String], create: bool) -> io::Result<OwnedFd> {
        let mut fd = self.root_fd()?;
        for d in dirs {
            let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_RDONLY;
            fd = match open_at(&fd, d, flags, 0) {
                Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
                    let n = cstr(d)?;
                    if unsafe { libc::mkdirat(fd.as_raw_fd(), n.as_ptr(), 0o755) } < 0 {
                        let e = errno();
                        if e.kind() != io::ErrorKind::AlreadyExists {
                            return Err(e);
                        }
                    }
                    open_at(&fd, d, flags, 0)?
                }
                r => r?,
            };
        }
        Ok(fd)
    }

    /// The file or directory at `parts`.
    pub fn find(&self, parts: &[String]) -> io::Result<Found> {
        let Some((last, dirs)) = parts.split_last() else {
            return Ok(Found::Dir(self.list(&self.root_fd()?)?));
        };
        let dir = self.walk(dirs, false)?;
        let fd = open_at(
            &dir,
            last,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        )?;
        let st = fstat(&fd)?;
        if is(&st, libc::S_IFDIR) {
            Ok(Found::Dir(self.list(&fd)?))
        } else if is(&st, libc::S_IFREG) {
            Ok(Found::File {
                file: File::from(fd),
                size: st.st_size as u64,
            })
        } else {
            Err(bad("not a file or a directory"))
        }
    }

    /// A directory's files and directories, by name; links and special
    /// files left out, and hidden ones (a write's temporary file).
    fn list(&self, dir: &OwnedFd) -> io::Result<Vec<Entry>> {
        let mut out = vec![];
        for e in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(st) = stat_at(dir, &name) else {
                continue;
            };
            let kind = if is(&st, libc::S_IFREG) {
                "file"
            } else if is(&st, libc::S_IFDIR) {
                "dir"
            } else {
                continue;
            };
            out.push(Entry {
                name,
                kind,
                size: if kind == "file" { st.st_size as u64 } else { 0 },
                modified: st.st_mtime,
            });
            if out.len() > MAX_ENTRIES {
                return Err(bad(format!("more than {MAX_ENTRIES} entries")));
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Where a write to `parts` goes: its directory (made as needed), its
    /// name, and a new hidden file beside it to write to.
    pub fn begin_write(&self, parts: &[String]) -> io::Result<Write> {
        let (last, dirs) = parts.split_last().ok_or_else(|| bad("a file's path"))?;
        let dir = self.walk(dirs, true)?;
        let mut b = [0u8; 6];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b)
            .map_err(|_| io::Error::other("no randomness"))?;
        let tmp = format!(
            ".{}.part",
            b.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let fd = open_at(
            &dir,
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o644,
        )?;
        Ok(Write {
            dir,
            tmp,
            name: last.clone(),
            file: Some(File::from(fd)),
        })
    }

    /// Removes the file, or empty directory, at `parts`.
    pub fn remove(&self, parts: &[String]) -> io::Result<()> {
        let (last, dirs) = parts
            .split_last()
            .ok_or_else(|| bad("not the drive itself"))?;
        let dir = self.walk(dirs, false)?;
        let st = stat_at(&dir, last)?;
        let flags = if is(&st, libc::S_IFDIR) {
            libc::AT_REMOVEDIR
        } else if is(&st, libc::S_IFREG) {
            0
        } else {
            return Err(bad("not a file or a directory"));
        };
        let n = cstr(last)?;
        if unsafe { libc::unlinkat(dir.as_raw_fd(), n.as_ptr(), flags) } < 0 {
            return Err(errno());
        }
        Ok(())
    }
}

/// A write under way: its temporary file, renamed into place by `finish`,
/// removed if dropped first.
pub struct Write {
    dir: OwnedFd,
    tmp: String,
    name: String,
    file: Option<File>,
}

impl Write {
    pub fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("unfinished")
    }

    /// Puts the file in place, over what had its name (not a directory).
    pub fn finish(mut self) -> io::Result<()> {
        let f = self.file.take().expect("unfinished");
        f.sync_all()?;
        drop(f);
        let (from, to) = (cstr(&self.tmp)?, cstr(&self.name)?);
        let fd = self.dir.as_raw_fd();
        if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } < 0 {
            return Err(errno());
        }
        self.tmp.clear();
        Ok(())
    }
}

impl Drop for Write {
    fn drop(&mut self) {
        if !self.tmp.is_empty() {
            if let Ok(n) = cstr(&self.tmp) {
                unsafe { libc::unlinkat(self.dir.as_raw_fd(), n.as_ptr(), 0) };
            }
        }
    }
}

/// Content type for a file served to a guest: text as text (HTML and
/// scripts among it, never run), images, PDFs, else bytes. As the office's
/// drive serves its files.
pub fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "md" | "markdown" | "txt" | "log" | "csv" | "tsv" | "json" | "yaml" | "yml" | "toml"
        | "rs" | "py" | "js" | "ts" | "go" | "html" | "css" | "sh" | "sql" => {
            "text/plain; charset=utf-8"
        }
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    fn drive(name: &str) -> (Files, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("zapper-wb-drive-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        (Files::new(&root), root)
    }

    fn p(s: &str) -> Vec<String> {
        components(s).unwrap()
    }

    #[test]
    fn a_path_is_cleaned_to_names_the_drive_takes() {
        assert_eq!(p("/inbox//a%20b.txt"), ["inbox", "a b.txt"]);
        assert_eq!(p("./inbox/./x"), ["inbox", "x"]);
        assert!(p("/").is_empty());
        for bad in [
            "/../etc",
            "/inbox/../../x",
            "/.ssh/x",
            "/a/.hidden",
            "/%2e%2e/x",
            "/a%00b",
            "/%ff",
        ] {
            assert!(components(bad).is_err(), "{bad}");
        }
        assert!(components(&"a/".repeat(40)).is_err());
    }

    #[test]
    fn files_are_written_whole_read_listed_and_removed() {
        let (d, root) = drive("rw");
        let mut w = d.begin_write(&p("outbox/s1/report.txt")).unwrap();
        w.file().write_all(b"hello").unwrap();
        // Not there until whole.
        assert!(!root.join("outbox/s1/report.txt").exists());
        w.finish().unwrap();
        match d.find(&p("outbox/s1/report.txt")).unwrap() {
            Found::File { mut file, size } => {
                let mut s = String::new();
                file.read_to_string(&mut s).unwrap();
                assert_eq!((s.as_str(), size), ("hello", 5));
            }
            Found::Dir(_) => panic!("a file"),
        }
        match d.find(&p("outbox/s1")).unwrap() {
            Found::Dir(e) => assert_eq!(
                e.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
                ["report.txt"]
            ),
            Found::File { .. } => panic!("a directory"),
        }
        // A write dropped before it finished leaves nothing.
        drop(d.begin_write(&p("outbox/s1/half.txt")).unwrap());
        assert_eq!(
            std::fs::read_dir(root.join("outbox/s1")).unwrap().count(),
            1
        );
        d.remove(&p("outbox/s1/report.txt")).unwrap();
        d.remove(&p("outbox/s1")).unwrap();
        assert!(d.remove(&p("outbox/missing")).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_link_anywhere_leads_nowhere() {
        let (d, root) = drive("links");
        let outside = root.with_extension("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "no").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("file")).unwrap();
        assert!(
            d.find(&p("out/secret")).is_err(),
            "through a linked directory"
        );
        assert!(d.find(&p("file")).is_err(), "a linked file");
        assert!(d.begin_write(&p("out/new")).is_err(), "writing through one");
        assert!(d.remove(&p("file")).is_err());
        // Listings leave links out.
        match d.find(&[]).unwrap() {
            Found::Dir(e) => assert!(e.is_empty(), "{e:?}"),
            Found::File { .. } => panic!(),
        }
        assert!(!outside.join("new").exists());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn what_could_run_in_a_browser_is_served_as_text() {
        assert_eq!(content_type("x.html"), "text/plain; charset=utf-8");
        assert_eq!(content_type("x.svg"), "application/octet-stream");
        assert_eq!(content_type("x.PDF"), "application/pdf");
    }
}
