use std::{
    fmt,
    fs::File,
    path::{Path, PathBuf},
};

pub struct StateLock {
    _file: File,
}

impl fmt::Debug for StateLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("StateLock").finish_non_exhaustive()
    }
}

impl StateLock {
    pub fn acquire(state_path: &Path) -> Result<Self, StateLockError> {
        let parent = state_path.parent().ok_or(StateLockError::InvalidPath)?;
        validate_lock_parent(parent)?;
        let lock_path = lock_path(state_path)?;
        acquire_platform_lock(&lock_path).map(|file| Self { _file: file })
    }
}

#[cfg(unix)]
fn validate_lock_parent(path: &Path) -> Result<(), StateLockError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path).map_err(|_| StateLockError::InvalidPath)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        return Err(StateLockError::UnsafeParentDirectory);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_lock_parent(_path: &Path) -> Result<(), StateLockError> {
    Err(StateLockError::UnsupportedPlatform)
}

fn lock_path(state_path: &Path) -> Result<PathBuf, StateLockError> {
    if state_path.as_os_str().is_empty() || state_path.file_name().is_none() {
        return Err(StateLockError::InvalidPath);
    }
    let mut value = state_path.as_os_str().to_owned();
    value.push(".lock");
    Ok(PathBuf::from(value))
}

#[cfg(unix)]
fn acquire_platform_lock(path: &Path) -> Result<File, StateLockError> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| StateLockError::UnsafeOrUnavailable)?;
    let metadata = file
        .metadata()
        .map_err(|_| StateLockError::UnsafeOrUnavailable)?;
    if !metadata.is_file()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
    {
        return Err(StateLockError::UnsafeOrUnavailable);
    }
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(file);
    }
    let error = std::io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        Err(StateLockError::AlreadyRunning)
    } else {
        Err(StateLockError::UnsafeOrUnavailable)
    }
}

#[cfg(not(unix))]
fn acquire_platform_lock(_path: &Path) -> Result<File, StateLockError> {
    Err(StateLockError::UnsupportedPlatform)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateLockError {
    InvalidPath,
    UnsafeOrUnavailable,
    UnsafeParentDirectory,
    AlreadyRunning,
    UnsupportedPlatform,
}

impl StateLockError {
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::InvalidPath => "invalid_state_path",
            Self::UnsafeOrUnavailable => "unsafe_or_unavailable_lock",
            Self::UnsafeParentDirectory => "unsafe_lock_directory",
            Self::AlreadyRunning => "writer_already_running",
            Self::UnsupportedPlatform => "unsupported_lock_platform",
        }
    }
}

impl fmt::Display for StateLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.kind())
    }
}

impl std::error::Error for StateLockError {}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory() -> PathBuf {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "inferlab-trust-renewer-lock-{}-{sequence}",
            std::process::id()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("test directory");
        path
    }

    #[test]
    fn excludes_a_second_writer_and_unlocks_on_drop() {
        let directory = test_directory();
        let state = directory.join("state.json");
        let first = StateLock::acquire(&state).expect("first lock");
        assert_eq!(
            StateLock::acquire(&state).unwrap_err(),
            StateLockError::AlreadyRunning
        );
        drop(first);
        StateLock::acquire(&state).expect("lock after release");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn rejects_unsafe_permissions_and_symlinks() {
        let directory = test_directory();
        let state = directory.join("state.json");
        let lock = directory.join("state.json.lock");
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&lock)
            .expect("lock fixture");
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            StateLock::acquire(&state).unwrap_err(),
            StateLockError::UnsafeOrUnavailable
        );
        fs::remove_file(&lock).expect("remove fixture");
        let target = directory.join("target");
        fs::write(&target, b"").expect("target");
        symlink(&target, &lock).expect("symlink");
        assert_eq!(
            StateLock::acquire(&state).unwrap_err(),
            StateLockError::UnsafeOrUnavailable
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
