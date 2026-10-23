use anyhow::Context;
use std::ffi::c_void;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, GetExplicitEntriesFromAclW, GetNamedSecurityInfoW,
    SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID,
    TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, CopySid, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, NO_INHERITANCE,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
};
use windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
        }
    }
}

fn current_user_sid() -> anyhow::Result<Vec<u8>> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .context("could not open the current process token")?;
    }
    let token = OwnedHandle(token);

    let mut required_bytes = 0;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut required_bytes) };
    anyhow::ensure!(required_bytes != 0, "could not size the current user token");

    let word_size = mem::size_of::<usize>();
    let word_count = (required_bytes as usize).div_ceil(word_size);
    let mut token_information = vec![0usize; word_count];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(token_information.as_mut_ptr().cast()),
            required_bytes,
            &mut required_bytes,
        )
        .context("could not read the current user token")?;
    }

    let token_user = unsafe { &*token_information.as_ptr().cast::<TOKEN_USER>() };
    let sid_length = unsafe { GetLengthSid(token_user.User.Sid) };
    anyhow::ensure!(sid_length != 0, "the current user SID is empty");
    let mut sid = vec![0u8; sid_length as usize];
    unsafe {
        CopySid(
            sid_length,
            PSID(sid.as_mut_ptr().cast()),
            token_user.User.Sid,
        )
        .context("could not copy the current user SID")?;
    }
    Ok(sid)
}

fn local_system_sid() -> anyhow::Result<Vec<u8>> {
    let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut sid_length = sid.len() as u32;
    unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(PSID(sid.as_mut_ptr().cast())),
            &mut sid_length,
        )
        .context("could not create the LocalSystem SID")?;
    }
    sid.truncate(sid_length as usize);
    Ok(sid)
}

fn explicit_access(sid: PSID) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS.0,
        grfAccessMode: SET_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: PWSTR(sid.0.cast()),
            ..TRUSTEE_W::default()
        },
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn verify_restriction(path: &Path, user_sid: PSID, system_sid: PSID) -> anyhow::Result<()> {
    let path = wide_path(path);
    let mut dacl = ptr::null_mut::<ACL>();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        GetNamedSecurityInfoW(
            PCWSTR(path.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        )
        .ok()
        .context("could not read back the IPC socket DACL")?;
    }
    let _descriptor = LocalAllocation(descriptor.0);
    anyhow::ensure!(!dacl.is_null(), "the IPC socket has a null DACL");

    let mut control = 0;
    let mut revision = 0;
    unsafe {
        GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)
            .context("could not read the IPC socket security descriptor control")?;
    }
    anyhow::ensure!(
        control & SE_DACL_PROTECTED.0 != 0,
        "the IPC socket DACL still inherits permissions"
    );

    let mut entry_count = 0;
    let mut entries = ptr::null_mut::<EXPLICIT_ACCESS_W>();
    unsafe {
        GetExplicitEntriesFromAclW(dacl, &mut entry_count, &mut entries)
            .ok()
            .context("could not enumerate the IPC socket DACL")?;
    }
    let _entries = LocalAllocation(entries.cast());
    anyhow::ensure!(
        entry_count == 2,
        "the IPC socket DACL has unexpected entries"
    );

    let entries = unsafe { std::slice::from_raw_parts(entries, entry_count as usize) };
    let mut has_user = false;
    let mut has_system = false;
    for entry in entries {
        anyhow::ensure!(
            entry.grfAccessMode == GRANT_ACCESS
                && entry.grfAccessPermissions == FILE_ALL_ACCESS.0
                && entry.grfInheritance == NO_INHERITANCE
                && entry.Trustee.TrusteeForm == TRUSTEE_IS_SID,
            "the IPC socket DACL contains an unexpected access rule"
        );
        let trustee_sid = PSID(entry.Trustee.ptstrName.0.cast());
        has_user |= unsafe { EqualSid(trustee_sid, user_sid) }.is_ok();
        has_system |= unsafe { EqualSid(trustee_sid, system_sid) }.is_ok();
    }
    anyhow::ensure!(
        has_user,
        "the IPC socket DACL does not grant the current user access"
    );
    anyhow::ensure!(
        has_system,
        "the IPC socket DACL does not grant LocalSystem access"
    );
    Ok(())
}

pub(crate) fn restrict_socket_path(path: &Path) -> anyhow::Result<()> {
    let mut user_sid = current_user_sid()?;
    let mut system_sid = local_system_sid()?;
    let user_sid = PSID(user_sid.as_mut_ptr().cast());
    let system_sid = PSID(system_sid.as_mut_ptr().cast());
    let entries = [explicit_access(user_sid), explicit_access(system_sid)];

    let mut dacl = ptr::null_mut::<ACL>();
    unsafe {
        SetEntriesInAclW(Some(&entries), None, &mut dacl)
            .ok()
            .context("could not create the IPC socket DACL")?;
    }
    let _dacl = LocalAllocation(dacl.cast());

    let path_wide = wide_path(path);
    unsafe {
        SetNamedSecurityInfoW(
            PCWSTR(path_wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
        .ok()
        .context("could not apply the IPC socket DACL")?;
    }
    verify_restriction(path, user_sid, system_sid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn socket_path_restriction_is_applied_and_verified() -> anyhow::Result<()> {
        let id = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("tacky-borders-acl-{}-{id}.tmp", std::process::id()));
        let _file = File::create(&path)?;

        let result = restrict_socket_path(&path);
        let remove_result = std::fs::remove_file(&path);
        result?;
        remove_result?;
        Ok(())
    }
}
