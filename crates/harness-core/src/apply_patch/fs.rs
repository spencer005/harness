use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io,
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::error::ApplyPatchError;

static TEMP_ORDINAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    File,
    Symlink,
    Directory,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileMetadata {
    kind: FileKind,
    mode: u32,
}

impl FileMetadata {
    fn from_metadata(metadata: std::fs::Metadata) -> Self {
        let file_type = metadata.file_type();
        let kind = if file_type.is_file() {
            FileKind::File
        } else if file_type.is_symlink() {
            FileKind::Symlink
        } else if file_type.is_dir() {
            FileKind::Directory
        } else {
            FileKind::Other
        };
        Self {
            kind,
            mode: metadata.mode() & 0o7777,
        }
    }

    pub(crate) fn kind(&self) -> FileKind {
        self.kind
    }

    pub(crate) fn mode(&self) -> u32 {
        self.mode
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadFile {
    contents: Vec<u8>,
    metadata: FileMetadata,
}

impl ReadFile {
    pub(crate) fn contents(&self) -> &[u8] {
        &self.contents
    }

    pub(crate) fn metadata(&self) -> &FileMetadata {
        &self.metadata
    }
}

#[derive(Debug)]
pub(crate) struct TempFile {
    path: PathBuf,
    file: File,
}

impl TempFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) trait PatchFileSystem {
    fn lstat(&self, path: &Path) -> io::Result<FileMetadata>;
    fn read_file_no_follow(&self, path: &Path) -> io::Result<ReadFile>;
    fn create_temp_near(&self, target: &Path) -> io::Result<TempFile>;
    fn write_all(&self, temp: &mut TempFile, contents: &[u8], mode: u32) -> io::Result<()>;
    fn ensure_parent_dir(&self, target: &Path) -> io::Result<()>;
    fn rename_replace(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn unlink_file_or_symlink(&self, path: &Path) -> io::Result<()>;
    fn fsync_parent_dir(&self, path: &Path) -> io::Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct UnixFileSystem;

impl PatchFileSystem for UnixFileSystem {
    fn lstat(&self, path: &Path) -> io::Result<FileMetadata> {
        Ok(FileMetadata::from_metadata(std::fs::symlink_metadata(
            path,
        )?))
    }

    fn read_file_no_follow(&self, path: &Path) -> io::Result<ReadFile> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let metadata = FileMetadata::from_metadata(file.metadata()?);
        if metadata.kind() != FileKind::File {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("expected a regular file: {}", path.display()),
            ));
        }
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;
        Ok(ReadFile { contents, metadata })
    }

    fn create_temp_near(&self, target: &Path) -> io::Result<TempFile> {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path has no parent directory: {}", target.display()),
            )
        })?;
        self.ensure_parent_dir(target)?;
        for _ in 0..128 {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let ordinal = TEMP_ORDINAL.fetch_add(1, Ordering::Relaxed);
            let name = format!(
                ".apply-patch-{}.{}.{now}.{ordinal}.tmp",
                std::process::id(),
                target
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("target")
            );
            let path = parent.join(name);
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => return Ok(TempFile { path, file }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "failed to allocate temporary file near {}",
                target.display()
            ),
        ))
    }

    fn write_all(&self, temp: &mut TempFile, contents: &[u8], mode: u32) -> io::Result<()> {
        temp.file
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
        temp.file.write_all(contents)?;
        temp.file.sync_all()
    }

    fn ensure_parent_dir(&self, target: &Path) -> io::Result<()> {
        let parent = target.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path has no parent directory: {}", target.display()),
            )
        })?;
        std::fs::create_dir_all(parent)
    }

    fn rename_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        rename_noreplace(from, to)
    }

    fn unlink_file_or_symlink(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn fsync_parent_dir(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path has no parent directory: {}", path.display()),
            )
        })?;
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY)
            .open(parent)?;
        dir.sync_all()
    }
}

fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let from = cstring_path(from)?;
    let to = cstring_path(to)?;
    // SAFETY: `from` and `to` are valid NUL-terminated C strings created from
    // Rust paths above, both directory file descriptors are AT_FDCWD, and the
    // kernel copies the path bytes during the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn cstring_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("path contains NUL byte: {}", path.display()),
        )
    })
}

pub(crate) fn ensure_file_or_symlink_for_delete<Fs: PatchFileSystem>(
    fs: &Fs,
    path: &Path,
) -> Result<(), ApplyPatchError> {
    let metadata = fs.lstat(path).map_err(|source| {
        ApplyPatchError::io(format!("Failed to inspect file {}", path.display()), source)
    })?;
    match metadata.kind() {
        FileKind::File | FileKind::Symlink => Ok(()),
        FileKind::Directory => Err(ApplyPatchError::Apply(format!(
            "Failed to delete {}; directory deletion is unsupported",
            path.display()
        ))),
        FileKind::Other => Err(ApplyPatchError::Apply(format!(
            "Failed to delete {}; unsupported file type",
            path.display()
        ))),
    }
}

pub(crate) fn path_exists<Fs: PatchFileSystem>(
    fs: &Fs,
    path: &Path,
) -> Result<bool, ApplyPatchError> {
    match fs.lstat(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ApplyPatchError::io(
            format!("Failed to inspect file {}", path.display()),
            source,
        )),
    }
}

pub(crate) fn read_regular_file<Fs: PatchFileSystem>(
    fs: &Fs,
    path: &Path,
) -> Result<ReadFile, ApplyPatchError> {
    let read_file = fs.read_file_no_follow(path).map_err(|source| {
        ApplyPatchError::io(format!("Failed to read file {}", path.display()), source)
    })?;
    if read_file.metadata().kind() != FileKind::File {
        return Err(ApplyPatchError::Apply(format!(
            "Failed to read file {}; expected a regular file",
            path.display()
        )));
    }
    Ok(read_file)
}

pub(crate) fn stage_file_contents<Fs: PatchFileSystem>(
    fs: &Fs,
    target: &Path,
    contents: &[u8],
    mode: u32,
) -> Result<TempFile, ApplyPatchError> {
    let mut temp = fs.create_temp_near(target).map_err(|source| {
        ApplyPatchError::io(
            format!("Failed to create temporary file near {}", target.display()),
            source,
        )
    })?;
    fs.write_all(&mut temp, contents, mode).map_err(|source| {
        let _ = std::fs::remove_file(temp.path());
        ApplyPatchError::io(
            format!("Failed to write file {}", temp.path().display()),
            source,
        )
    })?;
    Ok(temp)
}
