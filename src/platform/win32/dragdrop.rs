//! 拖出到外部程序：OLE `DoDragDrop` + `CF_HDROP`。
//!
//! 文件管理器把条目拖到资源管理器、桌面、别的程序上——接收方看到的是一份标准的
//! HDROP 文件列表，与从资源管理器拖出的一模一样。接收方**自己完成**复制 / 移动
//! （资源管理器对 CF_HDROP 的惯例），本方只负责提供路径与允许的效果。
//!
//! `DoDragDrop` 自带消息泵、阻塞到松开鼠标为止，**不能在事件回调栈里调**——窗口状态
//! 正被借用着，重入就是 `&mut` 别名（AGENTS.md 铁律 6）。调用方用
//! [`EventCtx::defer_blocking`](crate::core::EventCtx::defer_blocking) 把它排到分发返回之后。

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::{implement, Result as WinResult, BOOL, HRESULT};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_FORMATETC, DV_E_TYMED, E_NOTIMPL, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_OK,
};
use windows::Win32::System::Com::{
    IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, DATADIR_GET,
    DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, CF_HDROP, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{SHCreateStdEnumFmtEtc, DROPFILES};

use crate::platform::DragEffect;

/// `DROPFILES` 头 + 双 NUL 结尾的 UTF-16 路径列表（HDROP 的内存布局）。
pub(crate) fn hdrop_bytes(paths: &[impl AsRef<Path>]) -> Vec<u8> {
    let header = std::mem::size_of::<DROPFILES>();
    let mut wide: Vec<u16> = Vec::new();
    for p in paths {
        wide.extend(p.as_ref().as_os_str().encode_wide());
        wide.push(0);
    }
    wide.push(0);
    let mut buf = vec![0u8; header + wide.len() * 2];
    let df = DROPFILES {
        pFiles: header as u32,
        pt: Default::default(),
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    // SAFETY: DROPFILES 是 POD，按字节拷进缓冲区开头。
    unsafe {
        std::ptr::copy_nonoverlapping(
            &df as *const DROPFILES as *const u8,
            buf.as_mut_ptr(),
            header,
        );
    }
    for (i, w) in wide.iter().enumerate() {
        buf[header + i * 2..header + i * 2 + 2].copy_from_slice(&w.to_le_bytes());
    }
    buf
}

/// 把字节复制进一块新的 HGLOBAL（接收方负责释放）。
unsafe fn to_hglobal(bytes: &[u8]) -> WinResult<HGLOBAL> {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
    let p = GlobalLock(h);
    if p.is_null() {
        return Err(windows::core::Error::from_thread());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    let _ = GlobalUnlock(h);
    Ok(h)
}

fn hdrop_format() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// 只提供 CF_HDROP 一种格式的数据对象。
#[implement(IDataObject)]
struct FileData {
    bytes: Vec<u8>,
}

#[allow(non_snake_case)]
impl IDataObject_Impl for FileData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let hr = self.QueryGetData(pformatetcin);
        if hr.is_err() {
            return Err(hr.into());
        }
        // SAFETY: 数据在本线程里复制进一块新的 HGLOBAL，接收方拿走所有权。
        let h = unsafe { to_hglobal(&self.bytes)? };
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if pformatetc.is_null() {
            return DV_E_FORMATETC;
        }
        // SAFETY: 调用方保证指针有效。
        let f = unsafe { &*pformatetc };
        if f.cfFormat != CF_HDROP.0 || f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_FORMATETC;
        }
        if f.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return DV_E_TYMED;
        }
        S_OK
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if !pformatetcout.is_null() {
            // SAFETY: 调用方保证指针有效。
            unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> WinResult<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        // SAFETY: 标准枚举器，Shell 自己管理其生命周期。
        unsafe { SHCreateStdEnumFmtEtc(&[hdrop_format()]) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: windows::core::Ref<'_, IAdviseSink>,
    ) -> WinResult<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> WinResult<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// 拖动源：Esc 取消、松开左键落下、其余交默认光标。
#[implement(IDropSource)]
struct Source;

#[allow(non_snake_case)]
impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if grfkeystate & MK_LBUTTON == MODIFIERKEYS_FLAGS(0) {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

thread_local! {
    static OLE_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 把一组文件 / 目录拖出去（阻塞到松开鼠标）。返回接收方执行的效果；没落到任何
/// 地方或按了 Esc 返回 [`DragEffect::None`]。
///
/// `allow_move` 为 false 时只允许复制（接收方看到的光标没有"移动"态）。
///
/// 必须在 UI 线程、且**不在事件回调里**调用——见模块文档。
pub fn drag_files(paths: &[impl AsRef<Path>], allow_move: bool) -> DragEffect {
    if paths.is_empty() {
        return DragEffect::None;
    }
    // SAFETY: OLE 调用都在 UI 线程；OleInitialize 重复调用返回 S_FALSE 无害。
    unsafe {
        if !OLE_READY.with(|r| r.get()) {
            // 已被别人（rfd、COM）以别的公寓初始化时会失败，此时 DoDragDrop 多半仍可用。
            let _ = OleInitialize(None);
            OLE_READY.with(|r| r.set(true));
        }
        let data: IDataObject = FileData {
            bytes: hdrop_bytes(paths),
        }
        .into();
        let source: IDropSource = Source.into();
        let mut allowed = DROPEFFECT_COPY | DROPEFFECT_LINK;
        if allow_move {
            allowed |= DROPEFFECT_MOVE;
        }
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(&data, &source, allowed, &mut effect);
        if hr != DRAGDROP_S_DROP {
            return DragEffect::None;
        }
        if effect & DROPEFFECT_MOVE != DROPEFFECT_NONE {
            DragEffect::Move
        } else if effect & DROPEFFECT_COPY != DROPEFFECT_NONE {
            DragEffect::Copy
        } else if effect & DROPEFFECT_LINK != DROPEFFECT_NONE {
            DragEffect::Link
        } else {
            DragEffect::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdrop_layout() {
        let b = hdrop_bytes(&["C:\\a.txt", "C:\\b"]);
        let header = std::mem::size_of::<DROPFILES>();
        // pFiles 指向路径区起点；fWide = 1
        assert_eq!(
            u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize,
            header
        );
        let wide: Vec<u16> = b[header..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let s = String::from_utf16_lossy(&wide);
        assert_eq!(s, "C:\\a.txt\0C:\\b\0\0");
    }

    #[test]
    fn 空列表不进入_ole() {
        let none: [&str; 0] = [];
        assert_eq!(drag_files(&none, true), DragEffect::None);
    }
}
