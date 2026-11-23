use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;
use windows::core::PWSTR;
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS,
};
use windows::Win32::Foundation::{
    CloseHandle, RtlNtStatusToDosError, HANDLE, OBJECT_ATTRIBUTE_FLAGS, UNICODE_STRING,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ACCESS_RIGHTS, FILE_APPEND_DATA,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_RENAME_INFO,
    FILE_SHARE_DELETE, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::IO::IO_STATUS_BLOCK;

const SYNCHRONIZE: u32 = 0x0010_0000;
const DELETE: u32 = 0x0001_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RedirectPolicy {
    Follow,
    Refuse,
}

impl RedirectPolicy {
    fn open_options(self) -> NTCREATEFILE_CREATE_OPTIONS {
        match self {
            Self::Follow => NTCREATEFILE_CREATE_OPTIONS(0),
            Self::Refuse => FILE_OPEN_REPARSE_POINT,
        }
    }
}

static FORCE_REFUSE: AtomicBool = AtomicBool::new(false);
static PROCESS_POLICY: LazyLock<RedirectPolicy> = LazyLock::new(|| unsafe {
    let mut token = HANDLE::default();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
        return RedirectPolicy::Refuse;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    let result = GetTokenInformation(
        token,
        TokenElevation,
        Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut returned,
    );
    let _ = CloseHandle(token);
    if result.is_err() || elevation.TokenIsElevated != 0 {
        RedirectPolicy::Refuse
    } else {
        RedirectPolicy::Follow
    }
});

#[doc(hidden)]
pub fn force_refuse_for_tests() {
    FORCE_REFUSE.store(true, Ordering::Relaxed);
}

fn process_policy() -> RedirectPolicy {
    if FORCE_REFUSE.load(Ordering::Relaxed) {
        RedirectPolicy::Refuse
    } else {
        *PROCESS_POLICY
    }
}

pub struct AnchoredDir {
    handles: Vec<File>,
    path: PathBuf,
    policy: RedirectPolicy,
}

fn name_wide(name: &OsStr) -> io::Result<Vec<u16>> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one path component",
        ));
    }
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || wide.iter().any(|&c| c == 0 || c == b':' as u16)
        || wide.len() > u16::MAX as usize / 2
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid path component",
        ));
    }
    Ok(wide)
}

fn open_relative(
    parent: &File,
    name: &OsStr,
    path: &Path,
    access: u32,
    share: FILE_SHARE_MODE,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: NTCREATEFILE_CREATE_OPTIONS,
) -> io::Result<File> {
    let mut wide = name_wide(name)?;
    let length = (wide.len() * 2) as u16;
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: PWSTR(wide.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE(parent.as_raw_handle()),
        ObjectName: &name,
        Attributes: OBJECT_ATTRIBUTE_FLAGS(OBJ_CASE_INSENSITIVE),
        ..Default::default()
    };
    let mut handle = HANDLE::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_ACCESS_RIGHTS(access | SYNCHRONIZE | FILE_READ_ATTRIBUTES.0),
            &attributes,
            &mut status_block,
            None,
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if status.0 < 0 {
        let error = io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32);
        if error.kind() == io::ErrorKind::AlreadyExists {
            return Err(error);
        }
        return Err(io::Error::new(
            error.kind(),
            format!(
                "Cannot open {} (redirected, unsafe, or inaccessible): {error}",
                path.display()
            ),
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}

fn verify(file: &File, path: &Path, directory: bool, policy: RedirectPolicy) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|error| io::Error::other(format!("Cannot verify {}: {error}", path.display())))?;
    let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if is_directory != directory
        || (policy == RedirectPolicy::Refuse
            && (info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                || (!directory && info.nNumberOfLinks != 1)))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("Unsafe or redirected path: {}", path.display()),
        ));
    }
    Ok(())
}

impl AnchoredDir {
    pub fn open(root: &Path, components: &[&OsStr]) -> io::Result<Self> {
        Self::open_with(root, components, process_policy())
    }

    pub fn open_with(
        root: &Path,
        components: &[&OsStr],
        policy: RedirectPolicy,
    ) -> io::Result<Self> {
        Self::walk(root, components, policy, FILE_OPEN_IF)
    }

    fn walk(
        root: &Path,
        components: &[&OsStr],
        policy: RedirectPolicy,
        disposition: NTCREATEFILE_CREATE_DISPOSITION,
    ) -> io::Result<Self> {
        // The caller supplies the trusted profile or temporary root, which may itself be redirected.
        let root_handle = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0 | SYNCHRONIZE)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(root)?;
        let mut handles = vec![root_handle];
        let mut path = root.to_path_buf();
        for component in components {
            path.push(component);
            let parent = handles
                .last()
                .ok_or_else(|| io::Error::other("missing anchored directory handle"))?;
            let file = open_relative(
                parent,
                component,
                &path,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                disposition,
                FILE_DIRECTORY_FILE | policy.open_options(),
            )?;
            verify(&file, &path, true, policy)?;
            handles.push(file);
        }
        Ok(Self {
            handles,
            path,
            policy,
        })
    }

    pub fn for_path(path: &Path) -> io::Result<Self> {
        Self::for_path_with(path, FILE_OPEN_IF)
    }

    pub(crate) fn for_existing_path(path: &Path) -> io::Result<Self> {
        Self::for_path_with(path, FILE_OPEN)
    }

    fn for_path_with(
        path: &Path,
        disposition: NTCREATEFILE_CREATE_DISPOSITION,
    ) -> io::Result<Self> {
        let local = directories::BaseDirs::new()
            .map(|d| d.data_local_dir().to_path_buf())
            .unwrap_or_else(std::env::temp_dir);
        let temp = std::env::temp_dir();
        let root = if path.starts_with(local.join("leopardwm")) {
            &local
        } else {
            &temp
        };
        let relative = path.strip_prefix(root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is outside the trusted root", path.display()),
            )
        })?;
        let components: Vec<_> = relative.components().map(|c| c.as_os_str()).collect();
        Self::walk(root, &components, process_policy(), disposition)
    }

    fn file(
        &self,
        name: &OsStr,
        access: u32,
        share: FILE_SHARE_MODE,
        disposition: NTCREATEFILE_CREATE_DISPOSITION,
    ) -> io::Result<File> {
        let path = self.path.join(name);
        let parent = self
            .handles
            .last()
            .ok_or_else(|| io::Error::other("missing anchored directory handle"))?;
        let file = open_relative(
            parent,
            name,
            &path,
            access,
            share,
            disposition,
            FILE_NON_DIRECTORY_FILE | self.policy.open_options(),
        )?;
        verify(&file, &path, false, self.policy)?;
        Ok(file)
    }

    pub fn open_read(&self, name: &OsStr) -> io::Result<File> {
        self.file(
            name,
            FILE_READ_DATA.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
        )
    }

    pub fn open_append(&self, name: &OsStr) -> io::Result<File> {
        self.file(
            name,
            FILE_APPEND_DATA.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN_IF,
        )
    }

    pub fn open_write(&self, name: &OsStr) -> io::Result<File> {
        self.file(name, FILE_WRITE_DATA.0, FILE_SHARE_READ, FILE_OPEN_IF)
    }

    pub fn open_write_shared(&self, name: &OsStr) -> io::Result<File> {
        self.file(
            name,
            FILE_WRITE_DATA.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN_IF,
        )
    }

    pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
        self.file(name, FILE_WRITE_DATA.0, FILE_SHARE_READ, FILE_CREATE)
    }

    pub fn rename_replace(&self, from: &OsStr, to: &OsStr) -> io::Result<()> {
        if self.policy == RedirectPolicy::Follow {
            name_wide(from)?;
            name_wide(to)?;
            return std::fs::rename(self.path.join(from), self.path.join(to));
        }
        let source = self.file(
            from,
            DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
        )?;
        let name = name_wide(to)?;
        let size = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
        let mut buffer = vec![0u64; size.div_ceil(std::mem::size_of::<u64>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = true;
            (*info).RootDirectory = HANDLE(self.handles.last().unwrap().as_raw_handle());
            (*info).FileNameLength = (name.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                name.len(),
            );
            // SetFileInformationByHandle(FileRenameInfo) rejects a non-null RootDirectory here.
            let mut status_block = IO_STATUS_BLOCK::default();
            let status = NtSetInformationFile(
                HANDLE(source.as_raw_handle()),
                &mut status_block,
                info.cast(),
                size as u32,
                FileRenameInformation,
            );
            if status.0 < 0 {
                return Err(io::Error::from_raw_os_error(
                    RtlNtStatusToDosError(status) as i32
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    #[ignore = "requires permission to create file symlinks"]
    fn follow_policy_renames_symlink_without_moving_target() {
        use std::os::windows::fs::symlink_file;
        let root = std::env::temp_dir().join(format!("lwm-follow-symlink-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        fs::write(&target, b"preserve").unwrap();
        symlink_file(&target, root.join("current.log")).unwrap();
        let dir = AnchoredDir::open_with(&root, &[], RedirectPolicy::Follow).unwrap();
        let result = dir.rename_replace(OsStr::new("current.log"), OsStr::new("previous.log"));
        drop(dir);
        let target_contents = fs::read(&target).ok();
        let previous_is_link = fs::symlink_metadata(root.join("previous.log"))
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        fs::remove_dir_all(root).unwrap();
        assert!(result.is_ok(), "rename failed: {result:?}");
        assert_eq!(target_contents.as_deref(), Some(&b"preserve"[..]));
        assert!(
            previous_is_link,
            "rename moved the target instead of the link"
        );
    }

    #[test]
    fn follow_policy_writes_through_junction_and_hard_link() {
        let root = std::env::temp_dir().join(format!("lwm-follow-policy-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let link = root.join("link");
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let outside = root.join("outside");
        fs::write(&outside, b"old").unwrap();
        fs::hard_link(&outside, target.join("file.log")).unwrap();
        let dir =
            AnchoredDir::open_with(&root, &[OsStr::new("link")], RedirectPolicy::Follow).unwrap();
        let mut file = dir.open_write(OsStr::new("file.log")).unwrap();
        file.set_len(0).unwrap();
        file.write_all(b"new").unwrap();
        drop(file);
        dir.rename_replace(OsStr::new("file.log"), OsStr::new("old.log"))
            .unwrap();
        drop(dir);
        let contents = fs::read(&outside).unwrap();
        let renamed = fs::read(target.join("old.log")).unwrap();
        fs::remove_dir(link).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(contents, b"new");
        assert_eq!(renamed, b"new");
    }
}
