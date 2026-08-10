//! A one-ACE, `SE_DACL_PROTECTED` DACL applied at create.
//!
//! Three traps, in the order they bite:
//!
//! 1. The DACL has exactly one ACE — the current user's SID, `FILE_ALL_ACCESS`,
//!    no inheritance.
//! 2. **`SE_DACL_PROTECTED` is the whole ballgame.** Without it, ACEs inherited
//!    from `%LOCALAPPDATA%` (typically `Administrators`, sometimes `Users`) are
//!    *merged* with ours and the result is not `0700` — it is `0700` plus
//!    whatever the parent grants. Every "chmod on Windows" snippet gets this
//!    wrong.
//! 3. **Apply at create**, via `SECURITY_ATTRIBUTES` on `CreateDirectoryW` /
//!    `CreateFileW`. A create-then-harden window on the session log is a real
//!    read window.
//!
//! No policy lives here (rule 6): this module decides nothing about *which*
//! principal to grant, only how to express "the current user, and nobody else"
//! in Win32.

use super::{EffectiveAccess, PrivateFs};
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetTokenInformation,
    InitializeSecurityDescriptor, LookupAccountSidW, SetSecurityDescriptorControl,
    SetSecurityDescriptorDacl, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SECURITY_DESCRIPTOR_CONTROL, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL,
    FILE_GENERIC_READ,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// The ACE type that grants. Anything else in a DACL cannot widen a read.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsPrivateFs;

impl WindowsPrivateFs {
    pub const fn new() -> Self {
        Self
    }
}

impl crate::sealed::Sealed for WindowsPrivateFs {}

impl PrivateFs for WindowsPrivateFs {
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        let sid = current_user_sid()?;
        let acl = one_ace_dacl(sid.as_psid(), FILE_ALL_ACCESS)?;
        let mut sd = protected_descriptor(&acl)?;
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_mut_ptr(),
            bInheritHandle: 0,
        };
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated and outlives the call; `sa` points at
        // a descriptor whose DACL is owned by `acl` for the same scope.
        if unsafe { CreateDirectoryW(wide.as_ptr(), &mut sa) } == 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::AlreadyExists {
                // A pre-existing directory keeps whatever it had — there is no
                // umask to have narrowed it — so tighten rather than accept it.
                return self.harden_existing(path);
            }
            return Err(err);
        }
        Ok(())
    }

    fn create_file_new(&self, path: &Path) -> io::Result<File> {
        let sid = current_user_sid()?;
        let acl = one_ace_dacl(sid.as_psid(), FILE_ALL_ACCESS)?;
        let mut sd = protected_descriptor(&acl)?;
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.as_mut_ptr(),
            bInheritHandle: 0,
        };
        let wide = wide(path);
        // `CREATE_NEW` is the `O_EXCL`, and share mode 0 means no other opener
        // gets in between create and the caller's first write. SAFETY: as above.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                &mut sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh handle from `CreateFileW`, owned by nothing else.
        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    fn harden_existing(&self, path: &Path) -> io::Result<()> {
        let sid = current_user_sid()?;
        let acl = one_ace_dacl(sid.as_psid(), FILE_ALL_ACCESS)?;
        let mut wide = wide(path);
        // `PROTECTED_DACL_SECURITY_INFORMATION` is `SE_DACL_PROTECTED`'s
        // equivalent on this call: it detaches the object from the parent's
        // inheritable ACEs rather than merging with them.
        // SAFETY: NUL-terminated path, a live ACL, and null for every field the
        // information flags say we are not setting.
        let rc = unsafe {
            SetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl.as_ptr(),
                ptr::null_mut(),
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        Ok(())
    }

    fn effective_access(&self, path: &Path) -> io::Result<EffectiveAccess> {
        let me = current_user_sid()?;
        let mut wide = wide(path);
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        // SAFETY: out-params are live for the call; `sd` is freed below and is
        // what owns the memory `dacl` points into.
        let rc = unsafe {
            GetNamedSecurityInfoW(
                wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut sd,
            )
        };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let owned = LocalOwned(sd as *mut c_void);
        let other_readers = read_grants_other_than(dacl, &me)?;
        drop(owned);
        Ok(EffectiveAccess {
            owner_only: other_readers.is_empty(),
            other_readers,
        })
    }
}

/// Every principal but `me` that the DACL grants any read bit to.
///
/// A null DACL is the dangerous case and is reported as such rather than as
/// "no entries": a NULL DACL grants everything to everyone.
fn read_grants_other_than(dacl: *const ACL, me: &OwnedSid) -> io::Result<Vec<String>> {
    if dacl.is_null() {
        return Ok(vec!["everyone (the object has a NULL DACL)".to_string()]);
    }
    let mut info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` is non-null and came from the OS; `info` matches the class.
    if unsafe {
        GetAclInformation(
            dacl,
            (&raw mut info).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut out = Vec::new();
    for i in 0..info.AceCount {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `i` is below the count the OS just reported.
        if unsafe { GetAce(dacl, i, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `ace` points at an ACE header; the type byte is its first
        // field and is valid for every ACE variant.
        let header = unsafe { *(ace as *const u8) };
        if header != ACCESS_ALLOWED_ACE_TYPE {
            continue; // deny and audit ACEs cannot widen a read
        }
        // SAFETY: the type byte says this is an ACCESS_ALLOWED_ACE, whose
        // `SidStart` is the first `DWORD` of an inline SID.
        let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
        if allowed.Mask & FILE_GENERIC_READ == 0 {
            continue;
        }
        let sid = (&raw const allowed.SidStart) as PSID;
        // SAFETY: both SIDs are valid for the length the OS gave them.
        if unsafe { EqualSid(sid, me.as_psid()) } != 0 {
            continue;
        }
        out.push(account_name(sid));
    }
    Ok(out)
}

/// A human-readable name for a SID, falling back to a marker rather than
/// dropping the entry — an unresolvable SID still reads the file.
fn account_name(sid: PSID) -> String {
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain_len = domain.len() as u32;
    let mut kind = 0i32;
    // SAFETY: both buffers are live and their lengths are passed by pointer.
    let ok = unsafe {
        LookupAccountSidW(
            ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut kind,
        )
    };
    if ok == 0 {
        return "an unresolvable SID".to_string();
    }
    String::from_utf16_lossy(&name[..name_len as usize])
}

/// An owned `TOKEN_USER` blob, kept alive because the `PSID` points into it.
struct OwnedSid(Vec<u8>);

impl OwnedSid {
    fn as_psid(&self) -> PSID {
        // SAFETY: the buffer holds a `TOKEN_USER` whose first field is the SID
        // pointer the OS wrote.
        unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
    }
}

fn current_user_sid() -> io::Result<OwnedSid> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: a pseudo-handle for our own process and a live out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut needed = 0u32;
    // SAFETY: the deliberate zero-length probe call that reports the size.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed) };
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is exactly the size the OS just asked for.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    // SAFETY: a handle we opened and no longer need, on both paths.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(OwnedSid(buf))
}

/// A DACL with exactly one allow ACE, built by `SetEntriesInAclW` so the ACE
/// order is canonical. Never hand-assemble ACE order.
struct OwnedAcl(*mut ACL);

impl OwnedAcl {
    fn as_ptr(&self) -> *const ACL {
        self.0
    }
}

impl Drop for OwnedAcl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `SetEntriesInAclW` allocates with `LocalAlloc`.
            unsafe { LocalFree(self.0.cast()) };
        }
    }
}

fn one_ace_dacl(sid: PSID, access: u32) -> io::Result<OwnedAcl> {
    let mut ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: access,
        grfAccessMode: SET_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    };
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: one live entry, a null "existing ACL" meaning "build from
    // scratch", and a live out-param.
    let rc = unsafe { SetEntriesInAclW(1, &mut ea, ptr::null(), &mut acl) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(OwnedAcl(acl))
}

/// An absolute security descriptor carrying `acl` as its DACL, with
/// `SE_DACL_PROTECTED` set so no inherited ACE is merged in.
struct Descriptor(Box<SECURITY_DESCRIPTOR>);

impl Descriptor {
    fn as_mut_ptr(&mut self) -> *mut c_void {
        (&raw mut *self.0).cast()
    }
}

fn protected_descriptor(acl: &OwnedAcl) -> io::Result<Descriptor> {
    // SAFETY: zeroed is a valid starting state for the struct
    // `InitializeSecurityDescriptor` is about to fill in.
    let mut sd: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
    let ptr = (&raw mut *sd).cast();
    // SAFETY: a live, correctly sized descriptor.
    if unsafe { InitializeSecurityDescriptor(ptr, 1) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `acl` outlives every use of this descriptor at both call sites.
    if unsafe { SetSecurityDescriptorDacl(ptr, 1, acl.as_ptr() as *mut ACL, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Trap 2. Without this the parent's inheritable ACEs are merged in and the
    // object is not owner-only.
    // SAFETY: setting one control bit on a descriptor we just initialized.
    if unsafe {
        SetSecurityDescriptorControl(
            ptr,
            SE_DACL_PROTECTED as SECURITY_DESCRIPTOR_CONTROL,
            SE_DACL_PROTECTED as SECURITY_DESCRIPTOR_CONTROL,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Descriptor(sd))
}

/// `LocalFree` on drop, for the descriptors `GetNamedSecurityInfoW` allocates.
struct LocalOwned(*mut c_void);

impl Drop for LocalOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by the OS with `LocalAlloc`.
            unsafe { LocalFree(self.0) };
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Whether the object's DACL is detached from its parent's inheritable ACEs.
///
/// Windows-only, and not part of the [`PrivateFs`] contract, because Unix has
/// no counterpart: there is nothing to inherit from and nothing to protect
/// against. The test below is the one assertion with no Unix twin.
#[cfg(test)]
pub(crate) fn dacl_is_protected(path: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Security::GetSecurityDescriptorControl;
    let mut wide = wide(path);
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: live out-params; `sd` owns the memory and is freed below.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let owned = LocalOwned(sd as *mut c_void);
    let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
    let mut revision = 0u32;
    // SAFETY: `sd` is a valid descriptor for the duration of `owned`.
    let ok = unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
    drop(owned);
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(control & (SE_DACL_PROTECTED as SECURITY_DESCRIPTOR_CONTROL) != 0)
}

/// Re-attach the object to its parent's inheritable ACEs — the test's way of
/// loosening something so `harden_existing` has work to do.
#[cfg(test)]
pub(crate) fn allow_inheritance(path: &Path) -> io::Result<()> {
    let mut wide = wide(path);
    // No `PROTECTED_DACL_SECURITY_INFORMATION`, and a null DACL pointer with
    // `UNPROTECTED` semantics: inheritable ACEs flow back in.
    // SAFETY: NUL-terminated path; nulls for everything not being set.
    let rc = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION
                | windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
            ptr::null_mut(),
        )
    };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows-only assertion with no Unix counterpart: the object is
    /// `SE_DACL_PROTECTED`, not merely correct-looking. Verify no inherited ACE
    /// survived, because a merged-in `Administrators` ACE would leave the DACL
    /// *looking* right while granting a second reader.
    #[test]
    fn a_private_object_is_detached_from_inherited_aces() {
        let scratch = std::env::temp_dir().join(format!("hotl-dacl-{}", std::process::id()));
        crate::PRIVATE_FS.create_dir(&scratch).unwrap();
        assert!(dacl_is_protected(&scratch).unwrap());

        let file = scratch.join("secret");
        drop(crate::PRIVATE_FS.create_file_new(&file).unwrap());
        assert!(dacl_is_protected(&file).unwrap());

        // And the re-hardening path restores it after inheritance is let back
        // in.
        allow_inheritance(&file).unwrap();
        crate::PRIVATE_FS.harden_existing(&file).unwrap();
        assert!(dacl_is_protected(&file).unwrap());

        let _ = std::fs::remove_dir_all(&scratch);
    }
}
