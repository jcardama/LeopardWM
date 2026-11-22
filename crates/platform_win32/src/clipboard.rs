use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

pub fn set_text(owner: isize, text: &str) -> windows::core::Result<()> {
    let wide: Vec<u16> = text
        .replace('\0', "�")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * std::mem::size_of::<u16>())?;
        let ptr = GlobalLock(memory).cast::<u16>();
        if ptr.is_null() {
            let error = windows::core::Error::from_thread();
            let _ = GlobalFree(Some(memory));
            return Err(error);
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(memory);
        if let Err(error) = OpenClipboard(Some(HWND(owner as *mut _))) {
            let _ = GlobalFree(Some(memory));
            return Err(error);
        }
        let result = EmptyClipboard().and_then(|()| {
            SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0))).map(|_| ())
        });
        let _ = CloseClipboard();
        if result.is_err() {
            let _ = GlobalFree(Some(memory));
        }
        result
    }
}
