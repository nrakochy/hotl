//! `NtCreateFile` with `OBJECT_ATTRIBUTES.RootDirectory` — **this is `openat`**.
//!
//! There is no Win32 equivalent. `ReOpenFile` takes no name, and re-opening by
//! full path at each step reintroduces the exact TOCTOU the guard exists to
//! eliminate. So the NT layer it is.
//!
//! Two flags carry the containment:
//!
//! - **`FILE_OPEN_REPARSE_POINT`** is the `O_NOFOLLOW`: the open lands on the
//!   link itself rather than its target. Unlike `O_NOFOLLOW` it *succeeds*, so
//!   every open here re-reads the attributes off the handle it just got and
//!   refuses anything carrying `FILE_ATTRIBUTE_REPARSE_POINT`. The decision is
//!   made on the handle the caller then uses, which is the guard's whole rule.
//! - **`OBJ_CASE_INSENSITIVE`** is set deliberately. Leaving it unset gives
//!   case-*sensitive* matching, so `Foo` and `foo` would be different names to
//!   us and the same object to the filesystem — a containment mismatch. Match
//!   the OS.

use super::{DirHandle, Excl, GuardIo, NodeId, NodeKind, OpenMode};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FileRenameInformation, NtCreateFile, NtSetInformationFile, FILE_CREATE, FILE_DIRECTORY_FILE,
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_OVERWRITE_IF,
    FILE_SYNCHRONOUS_IO_NONALERT,
};
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE,
    OBJ_DONT_REPARSE, STATUS_SUCCESS, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileBasicInfo, FileDispositionInfo, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, SetFileInformationByHandle,
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_BASIC_INFO, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES,
    SYNCHRONIZE, VOLUME_NAME_DOS,
};

/// `STATUS_REPARSE_POINT_ENCOUNTERED` — what `OBJ_DONT_REPARSE` returns when
/// any component of a multi-component resolution is a reparse point. The
/// genuine `RESOLVE_NO_SYMLINKS` analogue.
const STATUS_REPARSE_POINT_ENCOUNTERED: i32 = 0xC000_050Bu32 as i32;
/// `STATUS_NOT_A_DIRECTORY` / `STATUS_OBJECT_PATH_NOT_FOUND` also surface when
/// a link stands where a directory was expected.
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;

pub struct WindowsDirHandle(OwnedHandle);

impl crate::sealed::Sealed for WindowsDirHandle {}

/// Everything a sibling process might reasonably hold. Withholding share rights
/// here would make the guard fail on ordinary files another tool has open,
/// which is a usability bug dressed as a security one — the containment comes
/// from *where* we can open, never from excluding other openers.
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().collect()
}

fn nt_error(status: i32) -> GuardIo {
    // SAFETY: a plain integer translation call with no pointers.
    let code = unsafe { RtlNtStatusToDosError(status) };
    let err = io::Error::from_raw_os_error(code as i32);
    if matches!(
        status,
        STATUS_REPARSE_POINT_ENCOUNTERED | STATUS_NOT_A_DIRECTORY
    ) {
        GuardIo::link(err)
    } else {
        GuardIo::io(err)
    }
}

/// `NtCreateFile` relative to `root`, naming exactly one component.
///
/// `root` is `None` only for the guard root itself, which is opened by its full
/// path — the one place a multi-component name is legal, and it is ours rather
/// than the model's.
fn nt_open(
    root: Option<HANDLE>,
    name: &mut [u16],
    access: u32,
    disposition: u32,
    options: u32,
    extra_obj_flags: u32,
) -> Result<OwnedHandle, GuardIo> {
    let byte_len = (name.len() * 2) as u16;
    let mut unicode = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: name.as_mut_ptr(),
    };
    let attrs = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root.unwrap_or(ptr::null_mut()),
        ObjectName: &mut unicode,
        Attributes: OBJ_CASE_INSENSITIVE | extra_obj_flags,
        SecurityDescriptor: ptr::null_mut(),
        SecurityQualityOfService: ptr::null_mut(),
    };
    let mut handle: HANDLE = ptr::null_mut();
    // SAFETY: zeroed is a valid `IO_STATUS_BLOCK`; the kernel fills it in.
    let mut iosb = unsafe { std::mem::zeroed() };
    // SAFETY: `name` outlives the call and backs `unicode`; `attrs` points only
    // at live locals; `root`, when `Some`, is a directory handle we own.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attrs,
            &mut iosb,
            ptr::null(),
            0,
            SHARE_ALL,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            ptr::null(),
            0,
        )
    };
    if status != STATUS_SUCCESS {
        return Err(nt_error(status));
    }
    // SAFETY: a fresh kernel handle owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as _) })
}

fn info_by_handle(h: HANDLE) -> Option<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: zeroed is a valid instance; the call fills it in.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: `h` is a handle we own and `info` is live for the call.
    if unsafe { GetFileInformationByHandle(h, &mut info) } == 0 {
        return None;
    }
    Some(info)
}

/// The refusal that makes `FILE_OPEN_REPARSE_POINT` equivalent to
/// `O_NOFOLLOW`: we opened the link rather than following it, so now refuse it.
///
/// **Do not use `Metadata::file_type().is_symlink()` here.** On Windows that is
/// true only for `IO_REPARSE_TAG_SYMLINK`; an NTFS junction reports
/// `is_symlink() == false` and `is_dir() == true`. Test the attribute bit and
/// refuse **every** tag — junction, app-exec-link, WCI container layer,
/// OneDrive placeholder alike.
fn refuse_if_reparse(h: &OwnedHandle) -> Result<(), GuardIo> {
    let Some(info) = info_by_handle(h.as_raw_handle() as HANDLE) else {
        return Err(GuardIo::last_os_error());
    };
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(GuardIo::link(io::Error::from(io::ErrorKind::InvalidInput)));
    }
    Ok(())
}

fn to_file(h: OwnedHandle) -> File {
    File::from(h)
}

impl DirHandle for WindowsDirHandle {
    /// The `FILE_ATTRIBUTE_*` bits. Read-only and hidden play the role Unix
    /// gives the mode: an atomic replace that dropped them would silently
    /// un-hide or un-protect the file it edited.
    type Attrs = u32;

    fn open_root(path: &Path) -> Result<Self, GuardIo> {
        // The NT namespace wants `\??\C:\...` rather than a Win32 path, and the
        // root is the one full path this module resolves. NT paths are
        // backslash-only, but a Win32 path may reach us with forward slashes
        // (`git rev-parse --show-toplevel` reports them, and that path seeds a
        // worktree floor) — the object manager would read those as literal
        // filename characters and fail with ERROR_PATH_NOT_FOUND. Normalize
        // before prefixing.
        let native = path.display().to_string().replace('/', "\\");
        let mut name = wide(OsStr::new(&format!(r"\??\{native}")));
        let h = nt_open(
            None,
            &mut name,
            FILE_READ_ATTRIBUTES | GENERIC_READ,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
            0,
        )?;
        Ok(Self(h))
    }

    fn open_child_dir(&self, name: &OsStr) -> Result<Self, GuardIo> {
        let mut n = wide(name);
        let h = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            FILE_READ_ATTRIBUTES | GENERIC_READ,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            0,
        )?;
        refuse_if_reparse(&h)?;
        Ok(Self(h))
    }

    fn make_child_dir(&self, name: &OsStr) -> Result<(), GuardIo> {
        let mut n = wide(name);
        nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            FILE_READ_ATTRIBUTES,
            FILE_CREATE,
            FILE_DIRECTORY_FILE,
            0,
        )?;
        Ok(())
    }

    fn open_child_file(&self, name: &OsStr, mode: OpenMode) -> Result<File, GuardIo> {
        let mut n = wide(name);
        let options = match mode {
            OpenMode::File => FILE_NON_DIRECTORY_FILE,
            OpenMode::Dir => FILE_DIRECTORY_FILE,
        } | FILE_OPEN_REPARSE_POINT;
        let h = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            GENERIC_READ,
            FILE_OPEN,
            options,
            0,
        )?;
        refuse_if_reparse(&h)?;
        Ok(to_file(h))
    }

    fn create_child_file(&self, name: &OsStr, excl: Excl) -> Result<File, GuardIo> {
        let mut n = wide(name);
        // `FILE_CREATE` fails if anything is there — including a symlink, which
        // is what keeps a create from writing through one. `FILE_OVERWRITE_IF`
        // pairs with `FILE_OPEN_REPARSE_POINT` so an existing link is opened
        // rather than followed, and then refused below.
        let disposition = match excl {
            Excl::Truncate => FILE_OVERWRITE_IF,
            Excl::MustNotExist => FILE_CREATE,
        };
        let h = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            GENERIC_WRITE | FILE_WRITE_ATTRIBUTES | FILE_READ_ATTRIBUTES,
            disposition,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            0,
        )?;
        refuse_if_reparse(&h)?;
        Ok(to_file(h))
    }

    fn rename_child(&self, from: &OsStr, to: &OsStr) -> Result<(), GuardIo> {
        let mut n = wide(from);
        let src = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            DELETE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT,
            0,
        )?;
        // `FILE_RENAME_INFO` takes a `RootDirectory` handle, which is what makes
        // this a real `renameat` rather than a path-based rename that could be
        // redirected between the open and the move.
        let target = wide(to);
        let bytes = size_of::<FILE_RENAME_INFO>() + (target.len() + 1) * 2;
        let mut buf = vec![0u8; bytes];
        // SAFETY: `buf` is at least `size_of::<FILE_RENAME_INFO>()` bytes and
        // correctly aligned (Vec<u8> from the global allocator is 8-aligned,
        // and the struct's alignment is 8).
        let info = unsafe { &mut *(buf.as_mut_ptr() as *mut FILE_RENAME_INFO) };
        info.Anonymous.ReplaceIfExists = true;
        info.RootDirectory = self.0.as_raw_handle() as HANDLE;
        info.FileNameLength = (target.len() * 2) as u32;
        // SAFETY: the trailing name buffer is the tail of the allocation sized
        // for it above.
        unsafe {
            ptr::copy_nonoverlapping(target.as_ptr(), info.FileName.as_mut_ptr(), target.len());
        }
        // The Win32 `SetFileInformationByHandle(FileRenameInfo, …)` rejects a
        // non-NULL `RootDirectory` with ERROR_INVALID_PARAMETER (87); only the
        // native `NtSetInformationFile(FileRenameInformation, …)` honours a
        // handle-relative rename. Same buffer, correct entry point.
        // SAFETY: zeroed is a valid `IO_STATUS_BLOCK`; the kernel fills it in.
        let mut iosb = unsafe { std::mem::zeroed() };
        // SAFETY: `src` is a handle we own opened with DELETE; `buf` is live and
        // its length is what we pass.
        let status = unsafe {
            NtSetInformationFile(
                src.as_raw_handle() as HANDLE,
                &mut iosb,
                buf.as_ptr().cast(),
                bytes as u32,
                FileRenameInformation,
            )
        };
        if status != STATUS_SUCCESS {
            return Err(nt_error(status));
        }
        Ok(())
    }

    fn unlink_child(&self, name: &OsStr) {
        let mut n = wide(name);
        let Ok(h) = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            DELETE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT,
            0,
        ) else {
            return;
        };
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: a handle we own opened with DELETE, and a live info struct.
        unsafe {
            SetFileInformationByHandle(
                h.as_raw_handle() as HANDLE,
                FileDispositionInfo,
                (&raw const info).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            );
        }
    }

    fn child_kind(&self, name: &OsStr) -> Option<NodeKind> {
        let mut n = wide(name);
        // `FILE_READ_ATTRIBUTES` only, and `FILE_OPEN_REPARSE_POINT`, so this
        // classifies without following and without needing read access.
        let h = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            FILE_READ_ATTRIBUTES,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT,
            0,
        )
        .ok()?;
        let info = info_by_handle(h.as_raw_handle() as HANDLE)?;
        // Reparse first: a junction is *also* a directory, and answering "Dir"
        // for one is precisely the bug this ordering prevents.
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Some(NodeKind::NotFollowable { tag: None });
        }
        if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Some(NodeKind::Dir);
        }
        Some(NodeKind::RegularFile)
    }

    fn child_attrs(&self, name: &OsStr) -> Option<Self::Attrs> {
        let mut n = wide(name);
        let h = nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            FILE_READ_ATTRIBUTES,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT,
            0,
        )
        .ok()?;
        Some(info_by_handle(h.as_raw_handle() as HANDLE)?.dwFileAttributes)
    }

    fn apply_attrs(&self, file: &File, attrs: Self::Attrs) -> io::Result<()> {
        // SAFETY: zeroed is valid; zero in a time field means "leave it".
        let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
        info.FileAttributes = attrs;
        // SAFETY: a handle we own and a live info struct of the stated size.
        let ok = unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle() as HANDLE,
                FileBasicInfo,
                (&raw const info).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn identity(&self) -> Result<NodeId, GuardIo> {
        id_from(self.0.as_raw_handle() as HANDLE).map_err(GuardIo::io)
    }

    fn resolve_beneath(&self, rel: &Path, mode: OpenMode) -> Result<Option<File>, GuardIo> {
        // `OBJ_DONT_REPARSE` (Win10 1607+) fails the whole resolution if *any*
        // component is a reparse point — a genuine `RESOLVE_NO_SYMLINKS`
        // analogue. `..` never reaches here; the caller strips it lexically
        // first, exactly as on Linux.
        // NT treats `/` as a literal filename char and rejects `.`, so
        // translate the portable rel path (which may use `/`, matching the Unix
        // `openat2` twin) into a `\`-separated native name. A `.`/root path
        // collapses to empty, which resolves to this handle itself.
        let native: PathBuf = rel
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(s) => Some(s),
                _ => None,
            })
            .collect();
        if native.as_os_str().is_empty() {
            return Ok(None);
        }
        let mut n = wide(native.as_os_str());
        let options = match mode {
            OpenMode::File => FILE_NON_DIRECTORY_FILE,
            OpenMode::Dir => FILE_DIRECTORY_FILE,
        } | FILE_OPEN_REPARSE_POINT;
        match nt_open(
            Some(self.0.as_raw_handle() as HANDLE),
            &mut n,
            GENERIC_READ,
            FILE_OPEN,
            options,
            OBJ_DONT_REPARSE,
        ) {
            Ok(h) => {
                refuse_if_reparse(&h)?;
                Ok(Some(to_file(h)))
            }
            // A refusal is a refusal. There is no "this kernel lacks it" arm:
            // `OBJ_DONT_REPARSE` predates the minimum supported build, so an
            // error here always means the resolution was denied.
            Err(e) => Err(e),
        }
    }

    fn into_file(self) -> File {
        File::from(self.0)
    }

    fn sync_name_durability(&self) -> Result<(), GuardIo> {
        // NTFS journals the rename's metadata, so the guarantee already holds
        // when the rename returns — see the trait doc. There is no directory
        // `fsync` to make, and claiming otherwise would be the no-op this
        // codebase forbids.
        Ok(())
    }
}

fn id_from(h: HANDLE) -> io::Result<NodeId> {
    // `FILE_ID_INFO`, not `GetFileInformationByHandle`'s 64-bit index: ReFS
    // needs 128 bits, which is why `NodeId::file` is not a `u64`.
    // SAFETY: zeroed is valid; the call fills it in.
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: a handle we own, and a live buffer of the stated size.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            h,
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(NodeId {
        volume: info.VolumeSerialNumber,
        file: u128::from_le_bytes(info.FileId.Identifier),
    })
}

pub(super) fn identity_of(file: &File) -> io::Result<NodeId> {
    id_from(file.as_raw_handle() as HANDLE)
}

pub(super) fn identity_at(path: &Path) -> io::Result<NodeId> {
    // `FILE_FLAG_OPEN_REPARSE_POINT` is the `AT_SYMLINK_NOFOLLOW`: identify the
    // link itself, never what it points at. `FILE_FLAG_BACKUP_SEMANTICS` is
    // what lets the same call name a directory.
    use std::os::windows::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    identity_of(&f)
}

pub(super) fn normalized_name(file: &File) -> io::Result<Option<PathBuf>> {
    let h = file.as_raw_handle() as HANDLE;
    // SAFETY: probe call with a null buffer, which reports the length.
    let needed = unsafe {
        GetFinalPathNameByHandleW(
            h,
            ptr::null_mut(),
            0,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u16; needed as usize];
    // SAFETY: `buf` is exactly the size the OS just asked for.
    let written = unsafe {
        GetFinalPathNameByHandleW(
            h,
            buf.as_mut_ptr(),
            buf.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(PathBuf::from(OsString::from_wide(
        &buf[..written as usize],
    ))))
}
