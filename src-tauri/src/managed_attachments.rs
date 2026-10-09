//! Descriptor-relative Studio attachment operations. Never re-resolve an
//! attacker-replaceable `session-files/<agent>` pathname after authorization.
#![cfg(unix)]
use anyhow::{ensure, Context, Result};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;

const MAX_READ_BYTES: u64 = 20 * 1024 * 1024;

fn name(value: &str) -> Result<CString> {
    ensure!(
        !value.is_empty()
            && value != "."
            && value != ".."
            && !value.contains('/')
            && !value.contains('\\'),
        "invalid attachment path component"
    );
    CString::new(value).context("NUL byte in attachment name")
}
fn open_child(parent: RawFd, child: &CStr) -> Result<File> {
    let fd = unsafe {
        libc::openat(
            parent,
            child.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
fn mkdir_child(parent: RawFd, child: &CStr) -> Result<()> {
    let rc = unsafe { libc::mkdirat(parent, child.as_ptr(), 0o700) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(err.into());
        }
    }
    Ok(())
}

pub(crate) struct ManagedFolder {
    dir: File,
}
impl ManagedFolder {
    pub(crate) fn open(base: &Path, namespace: &str, create: bool) -> Result<Option<Self>> {
        let base = std::fs::canonicalize(base).context("Studio app-data directory missing")?;
        let parent = File::open(&base)?;
        ensure!(
            parent.metadata()?.is_dir(),
            "app-data base is not a directory"
        );
        let root_name = name("session-files")?;
        if create {
            mkdir_child(parent.as_raw_fd(), &root_name)?;
        }
        let root = match open_child(parent.as_raw_fd(), &root_name) {
            Ok(folder) => folder,
            Err(error)
                if !create
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        let namespace_name = name(namespace)?;
        if create {
            mkdir_child(root.as_raw_fd(), &namespace_name)?;
        }
        let dir = match open_child(root.as_raw_fd(), &namespace_name) {
            Ok(folder) => folder,
            Err(error)
                if !create
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(error),
        };
        Ok(Some(Self { dir }))
    }
    fn file_name(raw: &str) -> Result<CString> {
        name(raw)
    }
    fn open_file(&self, raw: &str) -> Result<File> {
        let name = Self::file_name(raw)?;
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        ensure!(
            file.metadata()?.is_file(),
            "managed attachment must be regular file"
        );
        Ok(file)
    }
    pub(crate) fn entries(&self) -> Result<Vec<(String, u64)>> {
        // dup() shares a directory's seek offset: a second enumeration would
        // unexpectedly return zero entries. Open "." relative to this trusted
        // descriptor to obtain an independently positioned directory stream.
        let dot = CString::new(".")?;
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                dot.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::last_os_error().into());
        }
        let mut rows = Vec::new();
        loop {
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                break;
            }
            let filename = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let Ok(filename) = filename.to_str() else {
                continue;
            };
            if filename == "." || filename == ".." {
                continue;
            }
            if let Ok(file) = self.open_file(filename) {
                if let Ok(meta) = file.metadata() {
                    rows.push((filename.to_string(), meta.len()));
                }
            }
        }
        unsafe { libc::closedir(stream) };
        Ok(rows)
    }
    pub(crate) fn read(&self, filename: &str) -> Result<Vec<u8>> {
        let file = self.open_file(filename)?;
        ensure!(
            file.metadata()?.len() <= MAX_READ_BYTES,
            "managed attachment exceeds size limit"
        );
        let mut data = Vec::new();
        file.take(MAX_READ_BYTES + 1).read_to_end(&mut data)?;
        ensure!(
            data.len() as u64 <= MAX_READ_BYTES,
            "managed attachment grew beyond size limit"
        );
        Ok(data)
    }
    pub(crate) fn remove(&self, filename: &str) -> Result<()> {
        // Unlinking a symlink, even if swapped after inspection, only removes
        // the link itself; it cannot follow a symlink to delete an external file.
        self.open_file(filename)?;
        let name = Self::file_name(filename)?;
        let rc = unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.dir.sync_all()?;
        Ok(())
    }
    pub(crate) fn create(&self, filename: &str, bytes: &[u8]) -> Result<()> {
        ensure!(
            !bytes.is_empty() && bytes.len() as u64 <= MAX_READ_BYTES,
            "invalid managed attachment size"
        );
        let name = Self::file_name(filename)?;
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) };
            return Err(error.into());
        }
        drop(file);
        self.dir.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn operations_remain_bound_to_original_dir_after_parent_symlink_swap() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("managed-fd-swap-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let outer = base.join("outside");
        std::fs::create_dir_all(&outer).unwrap();
        let folder = ManagedFolder::open(&base, "agent-safe", true)
            .unwrap()
            .unwrap();
        folder
            .create("studio-file-aaaa-11-safe.txt", b"inside")
            .unwrap();
        let root = base.join("session-files");
        let original = base.join("saved-root");
        std::fs::rename(&root, &original).unwrap();
        symlink(&outer, &root).unwrap();
        assert!(ManagedFolder::open(&base, "agent-safe", false).is_err());
        assert!(ManagedFolder::open(&base, "agent-safe", true).is_err());
        // The opened directory survives the path replacement, securely.
        folder
            .create("studio-file-aaaa-22-later.txt", b"still inside")
            .unwrap();
        assert_eq!(
            folder.read("studio-file-aaaa-22-later.txt").unwrap(),
            b"still inside"
        );
        assert_eq!(folder.entries().unwrap().len(), 2);
        assert_eq!(
            folder.entries().unwrap().len(),
            2,
            "repeated directory enumeration must not inherit a consumed offset"
        );
        folder.remove("studio-file-aaaa-11-safe.txt").unwrap();
        assert!(!outer.join("agent-safe").exists());
        assert!(original
            .join("agent-safe")
            .join("studio-file-aaaa-22-later.txt")
            .exists());
        std::fs::remove_file(&root).unwrap();
        std::fs::rename(&original, &root).unwrap();
        drop(folder);
        std::fs::remove_dir_all(&base).unwrap();
    }
    #[test]
    fn attachment_identity_and_size_bounds_fail_closed() {
        let base = std::env::temp_dir().join(format!("managed-fd-size-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let folder = ManagedFolder::open(&base, "agent-safe", true)
            .unwrap()
            .unwrap();
        assert!(folder.create("../outside", b"no").is_err());
        assert!(folder.create("studio-file-1-1-empty", b"").is_err());
        folder.create("studio-file-1-1-data", b"first").unwrap();
        assert!(folder
            .create("studio-file-1-1-data", b"replacement")
            .is_err());
        assert_eq!(folder.read("studio-file-1-1-data").unwrap(), b"first");
        let path = base.join("session-files/agent-safe/studio-file-1-1-large");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_READ_BYTES + 1)
            .unwrap();
        assert!(folder.read("studio-file-1-1-large").is_err());
        drop(folder);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
