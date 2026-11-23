use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Component, Path, PathBuf};
use windows::core::PWSTR;
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT,
    FILE_SYNCHRONOUS_IO_NONALERT, NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS,
};
use windows::Win32::Foundation::{
    RtlNtStatusToDosError, HANDLE, OBJECT_ATTRIBUTE_FLAGS, UNICODE_STRING,
};
use windows::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ACCESS_RIGHTS, FILE_APPEND_DATA,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;

const SYNCHRONIZE: u32 = 0x0010_0000;
const DELETE: u32 = 0x0001_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;

pub struct AnchoredDir {
    handles: Vec<File>,
    path: PathBuf,
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
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
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

fn verify(file: &File, path: &Path, directory: bool) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(|error| io::Error::other(format!("Cannot verify {}: {error}", path.display())))?;
    let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || is_directory != directory
        || (!directory && info.nNumberOfLinks != 1)
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
            let file = open_relative(
                handles.last().unwrap(),
                component,
                &path,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN_IF,
                FILE_DIRECTORY_FILE,
            )?;
            verify(&file, &path, true)?;
            handles.push(file);
        }
        Ok(Self { handles, path })
    }

    pub fn for_path(path: &Path) -> io::Result<Self> {
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
        Self::open(root, &components)
    }

    fn file(
        &self,
        name: &OsStr,
        access: u32,
        share: FILE_SHARE_MODE,
        disposition: NTCREATEFILE_CREATE_DISPOSITION,
    ) -> io::Result<File> {
        let path = self.path.join(name);
        let file = open_relative(
            self.handles.last().unwrap(),
            name,
            &path,
            access,
            share,
            disposition,
            FILE_NON_DIRECTORY_FILE,
        )?;
        verify(&file, &path, false)?;
        Ok(file)
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
