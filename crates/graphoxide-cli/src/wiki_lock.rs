//! Nonblocking serialization for complete direct-source operations.

use anyhow::{ensure, Context as _, Result};
use fs2::FileExt as _;
use std::{fs, io::Write as _, path::Path};

#[cfg(any(windows, test))]
const LOCK_NAME: &str = "direct-source.lock";

/// Holds the knowledgebase operation lock until dropped. The empty lock file
/// stays in place so waiting callers always coordinate on the same inode.
#[derive(Debug)]
pub struct DirectSourceOperation {
    _file: fs::File,
    _runtime: fs::File,
    _root: fs::File,
}

/// Serialize CLI/MCP source operations and preview materialization. This never
/// waits for a provider-owned operation; a busy knowledgebase must be retried.
pub fn acquire_source_operation(root: &Path) -> Result<DirectSourceOperation> {
    let metadata = fs::symlink_metadata(root).context("inspect knowledgebase operation root")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "knowledgebase operation root must be a real directory"
    );
    #[cfg(windows)]
    ensure!(
        !is_reparse(&metadata),
        "knowledgebase operation root must not be a link"
    );
    let root = root
        .canonicalize()
        .context("resolve knowledgebase operation root")?;
    let (root_directory, runtime) = open_runtime(&root)?;
    let file = open_lock(&root, &runtime)?;
    validate_lock(&file)?;
    file.try_lock_exclusive().map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock
            || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        {
            anyhow::anyhow!(
                "knowledgebase is busy with another source operation; retry when it finishes"
            )
        } else {
            anyhow::anyhow!("acquire knowledgebase operation lock failed: {error}")
        }
    })?;
    validate_lock(&file)?;
    ensure_runtime_ignored(&root, &runtime)?;
    Ok(DirectSourceOperation {
        _file: file,
        _runtime: runtime,
        _root: root_directory,
    })
}

#[cfg(unix)]
fn open_runtime(root: &Path) -> Result<(fs::File, fs::File)> {
    use std::os::unix::{fs::OpenOptionsExt as _, io::AsRawFd as _};
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .context("open knowledgebase operation root")?;
    // SAFETY: the directory descriptor remains live and the literal name is NUL-terminated.
    let created = unsafe { libc::mkdirat(directory.as_raw_fd(), c".graphoxide".as_ptr(), 0o700) };
    if created != 0 {
        let error = std::io::Error::last_os_error();
        ensure!(
            error.kind() == std::io::ErrorKind::AlreadyExists,
            "create knowledgebase runtime directory failed"
        );
    }
    let runtime = openat(
        &directory,
        c".graphoxide",
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    )?;
    Ok((directory, runtime))
}

#[cfg(unix)]
fn openat(
    directory: &fs::File,
    name: &std::ffi::CStr,
    flags: i32,
    mode: libc::mode_t,
) -> Result<fs::File> {
    use std::os::unix::io::{AsRawFd as _, FromRawFd as _};
    // SAFETY: the parent descriptor and NUL-terminated child name are valid for this call;
    // successful descriptors are transferred exactly once to File.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error())
            .context("open safe knowledgebase runtime entry");
    }
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_lock(_root: &Path, runtime: &fs::File) -> Result<fs::File> {
    openat(
        runtime,
        c"direct-source.lock",
        libc::O_RDWR | libc::O_CREAT,
        0o600,
    )
}

#[cfg(unix)]
fn validate_lock(file: &fs::File) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = file
        .metadata()
        .context("inspect knowledgebase operation lock")?;
    ensure!(
        metadata.is_file() && metadata.nlink() == 1 && metadata.len() == 0,
        "knowledgebase operation lock must be an empty, singly linked regular file"
    );
    Ok(())
}

#[cfg(unix)]
fn ensure_runtime_ignored(_root: &Path, runtime: &fs::File) -> Result<()> {
    match openat(
        runtime,
        c".gitignore",
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        0o600,
    ) {
        Ok(mut ignore) => {
            ignore
                .write_all(b"*\n")
                .context("ignore knowledgebase runtime entries")?;
            ignore
                .sync_all()
                .context("sync knowledgebase runtime ignore rule")?;
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            let ignore = openat(runtime, c".gitignore", libc::O_RDONLY, 0)?;
            validate_runtime_ignore(&ignore)?;
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn validate_runtime_ignore(file: &fs::File) -> Result<()> {
    let metadata = file
        .metadata()
        .context("inspect knowledgebase runtime ignore file")?;
    ensure!(
        metadata.is_file() && metadata.len() <= 4096,
        "knowledgebase runtime ignore file must be a bounded regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        ensure!(
            metadata.nlink() == 1,
            "knowledgebase runtime ignore file must not be multiply linked"
        );
    }
    #[cfg(windows)]
    ensure!(
        windows_information(file)?.nNumberOfLinks == 1 && !is_reparse(&metadata),
        "knowledgebase runtime ignore file must not be linked"
    );
    Ok(())
}

#[cfg(windows)]
fn open_runtime(root: &Path) -> Result<(fs::File, fs::File)> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let open_directory = |path: &Path| -> Result<fs::File> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .context("open knowledgebase runtime directory")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_dir() && !is_reparse(&metadata),
            "knowledgebase runtime directory must not be a link"
        );
        Ok(file)
    };
    let directory = open_directory(root)?;
    let path = root.join(".graphoxide");
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("create knowledgebase runtime directory"),
    }
    let runtime = open_directory(&path)?;
    Ok((directory, runtime))
}

#[cfg(windows)]
fn lock_options() -> fs::OpenOptions {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let mut options = fs::OpenOptions::new();
    options
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    options
}

#[cfg(windows)]
fn open_lock(root: &Path, _runtime: &fs::File) -> Result<fs::File> {
    lock_options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".graphoxide").join(LOCK_NAME))
        .context("open knowledgebase operation lock")
}

#[cfg(windows)]
fn is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(windows)]
fn windows_information(
    file: &fs::File,
) -> Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the borrowed file handle and initialized writable output remain valid for the call.
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } != 0,
        "inspect knowledgebase operation lock handle failed"
    );
    Ok(information)
}

#[cfg(windows)]
fn validate_lock(file: &fs::File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && !is_reparse(&metadata)
            && metadata.len() == 0
            && windows_information(file)?.nNumberOfLinks == 1,
        "knowledgebase operation lock must be an empty, singly linked regular file"
    );
    Ok(())
}

#[cfg(windows)]
fn ensure_runtime_ignored(root: &Path, _runtime: &fs::File) -> Result<()> {
    let path = root.join(".graphoxide/.gitignore");
    match lock_options().write(true).create_new(true).open(&path) {
        Ok(mut ignore) => {
            ignore
                .write_all(b"*\n")
                .context("ignore knowledgebase runtime entries")?;
            ignore
                .sync_all()
                .context("sync knowledgebase runtime ignore rule")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_runtime_ignore(&lock_options().read(true).open(path)?)?;
        }
        Err(error) => return Err(error).context("create knowledgebase runtime ignore rule"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_lock_is_nonblocking_and_reuses_an_empty_file_after_release() {
        let root = tempfile::tempdir().expect("knowledgebase");
        let first = acquire_source_operation(root.path()).expect("first operation");
        let path = root.path().join(".graphoxide").join(LOCK_NAME);
        assert_eq!(fs::metadata(&path).expect("empty lock").len(), 0);
        let error =
            acquire_source_operation(root.path()).expect_err("second operation must be busy");
        assert!(format!("{error:#}").contains("busy"));
        drop(first);
        let next = acquire_source_operation(root.path()).expect("retry after release");
        assert_eq!(fs::metadata(&path).expect("stable empty lock").len(), 0);
        drop(next);
        assert_eq!(fs::read(path).expect("stable empty lock"), b"");
    }

    #[test]
    fn operation_lock_preserves_existing_runtime_ignore_patterns() {
        let root = tempfile::tempdir().expect("knowledgebase");
        fs::write(root.path().join(".gitignore"), b".graphoxide/\n").expect("runtime exclusion");
        fs::create_dir(root.path().join(".graphoxide")).expect("runtime directory");
        let path = root.path().join(".graphoxide/.gitignore");
        let original = b"# Existing local cache policy\ncache/\n*.tmp\n";
        fs::write(&path, original).expect("existing runtime ignore rules");

        let _operation =
            acquire_source_operation(root.path()).expect("existing rules are supported");

        assert_eq!(fs::read(path).expect("preserved rules"), original);
        assert_eq!(
            fs::read(root.path().join(".gitignore")).expect("root rules"),
            b".graphoxide/\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn operation_lock_rejects_symlinked_runtime_or_lock_and_hardlinked_lock() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("knowledgebase");
        let outside = tempfile::tempdir().expect("outside");
        symlink(outside.path(), root.path().join(".graphoxide")).expect("runtime symlink");
        assert!(acquire_source_operation(root.path()).is_err());
        assert!(fs::read_dir(outside.path())
            .expect("outside directory")
            .next()
            .is_none());
        fs::remove_file(root.path().join(".graphoxide")).expect("remove symlink");
        fs::create_dir(root.path().join(".graphoxide")).expect("runtime");
        let target = outside.path().join("target");
        fs::write(&target, b"").expect("external empty file");
        let lock = root.path().join(".graphoxide").join(LOCK_NAME);
        symlink(&target, &lock).expect("lock symlink");
        assert!(acquire_source_operation(root.path()).is_err());
        fs::remove_file(&lock).expect("remove lock symlink");
        fs::hard_link(&target, &lock).expect("lock hardlink");
        assert!(acquire_source_operation(root.path()).is_err());
        assert_eq!(fs::read(target).expect("external file"), b"");
    }
}
