use std::ffi::c_void;
use std::net::SocketAddrV4;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::Networking::WinSock::AF_INET;
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, IsValidSid, TokenUser, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn token_user(process: HANDLE) -> Option<Vec<usize>> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
        let token = OwnedHandle(token);
        let mut len = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut len);
        if len < std::mem::size_of::<TOKEN_USER>() as u32 {
            return None;
        }
        let mut buffer = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            len,
            &mut len,
        )
        .ok()?;
        Some(buffer)
    }
}

fn same_user(left: &[usize], right: &[usize]) -> bool {
    if std::mem::size_of_val(left) < std::mem::size_of::<TOKEN_USER>()
        || std::mem::size_of_val(right) < std::mem::size_of::<TOKEN_USER>()
    {
        return false;
    }
    unsafe {
        let a = (*(left.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let b = (*(right.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        IsValidSid(a).as_bool() && IsValidSid(b).as_bool() && EqualSid(a, b).is_ok()
    }
}

fn peer_pid(peer: SocketAddrV4, local: SocketAddrV4) -> Option<u32> {
    unsafe {
        let mut len = 0u32;
        let _ = GetExtendedTcpTable(
            None,
            &mut len,
            false,
            u32::from(AF_INET.0),
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
        if len < 4 {
            return None;
        }
        let mut buffer = vec![0u32; (len as usize).div_ceil(4)];
        if GetExtendedTcpTable(
            Some(buffer.as_mut_ptr().cast()),
            &mut len,
            false,
            u32::from(AF_INET.0),
            TCP_TABLE_OWNER_PID_ALL,
            0,
        ) != 0
        {
            return None;
        }
        let count = buffer[0] as usize;
        let row_words = std::mem::size_of::<MIB_TCPROW_OWNER_PID>() / 4;
        if count > (buffer.len() - 1) / row_words {
            return None;
        }
        let rows = std::slice::from_raw_parts(
            buffer.as_ptr().add(1).cast::<MIB_TCPROW_OWNER_PID>(),
            count,
        );
        let mut matches = rows.iter().filter(|row| {
            row.dwLocalAddr == u32::from_ne_bytes(peer.ip().octets())
                && row.dwRemoteAddr == u32::from_ne_bytes(local.ip().octets())
                && u16::from_be(row.dwLocalPort as u16) == peer.port()
                && u16::from_be(row.dwRemotePort as u16) == local.port()
        });
        let pid = matches.next()?.dwOwningPid;
        if matches.next().is_some() {
            return None;
        }
        Some(pid)
    }
}

pub(super) fn same_user_peer(peer: SocketAddrV4, local: SocketAddrV4) -> bool {
    let Some(pid) = peer_pid(peer, local) else {
        return false;
    };
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let process = OwnedHandle(process);
        let Some(peer_user) = token_user(process.0) else {
            return false;
        };
        let Some(daemon_user) = token_user(GetCurrentProcess()) else {
            return false;
        };
        same_user(&peer_user, &daemon_user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sid_comparison_accepts_current_user_and_rejects_missing_token() {
        let current = token_user(unsafe { GetCurrentProcess() }).unwrap();
        assert!(same_user(&current, &current));
        assert!(!same_user(&current, &[]));
        unsafe {
            use windows::core::w;
            use windows::Win32::Foundation::{LocalFree, HLOCAL};
            use windows::Win32::Security::Authorization::ConvertStringSidToSidW;
            let mut sid = windows::Win32::Security::PSID::default();
            ConvertStringSidToSidW(w!("S-1-1-0"), &mut sid).unwrap();
            let mut different = vec![
                0usize;
                std::mem::size_of::<TOKEN_USER>()
                    .div_ceil(std::mem::size_of::<usize>())
            ];
            (*different.as_mut_ptr().cast::<TOKEN_USER>()).User.Sid = sid;
            assert!(!same_user(&current, &different));
            let _ = LocalFree(Some(HLOCAL(sid.0)));
        }
    }
}
