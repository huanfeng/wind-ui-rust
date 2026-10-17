//! 内联组合串的显示属性：从 TSF 读输入法配置的下划线线型与颜色。
//!
//! 本库是 IMM32 宿主，IMM 只给逐字的粗分类（`GCS_COMPATTR`：输入中 / 目标分句……），
//! 输入法真正配置的样式（`TF_DISPLAYATTRIBUTE`：点线还是实线、粗细、颜色）只在 TSF 里。
//! TSF 输入法经 CUAS 跟 IMM32 宿主打交道时，CUAS 在本线程上建了一个临时上下文
//! （`TS_SS_TRANSITORY`），里面的文本就是组合串本身，偏移与 IMM 的 UTF-16 下标一致。
//! 在 `WM_IME_COMPOSITION` 里同步开一个只读编辑会话，就能逐段读到显示属性。
//!
//! 任何一步失败（纯 IMM 输入法没有 TSF 上下文、COM 未初始化、上下文文本对不上……）
//! 都返回空，控件退回默认画法。
use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;

use windows::core::{implement, Interface, Result, GUID};
use windows::Win32::Graphics::Gdi::{GetSysColor, SYS_COLOR_INDEX};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Variant::VT_I4;
use windows::Win32::UI::TextServices::{
    CLSID_TF_CategoryMgr, CLSID_TF_DisplayAttributeMgr, CLSID_TF_ThreadMgr, ITfCategoryMgr,
    ITfContext, ITfDisplayAttributeMgr, ITfEditSession, ITfEditSession_Impl, ITfProperty, ITfRange,
    ITfRangeACP, ITfThreadMgr, GUID_PROP_ATTRIBUTE, TF_ANCHOR_END, TF_CT_COLORREF, TF_CT_SYSCOLOR,
    TF_DA_COLOR, TF_DISPLAYATTRIBUTE, TF_ES_READ, TF_ES_SYNC, TF_LS_DASH, TF_LS_DOT, TF_LS_NONE,
    TF_LS_SOLID, TF_LS_SQUIGGLE,
};

use crate::event::{PreeditSpan, UnderlineStyle};
use crate::geometry::Color;

/// 一段显示属性，偏移是 UTF-16 码元（临时上下文的 ACP）。颜色已换成 [`Color`]。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TsfRange {
    pub start: i32,
    pub len: i32,
    pub style: PreeditSpan,
}

/// 把读到的分段换成字符下标的 [`PreeditSpan`]。上下文文本必须与 IMM 的组合串逐码元相同，
/// 否则偏移的参照系不同，宁可不用。越界的段裁掉，空段丢掉。
pub(super) fn spans_from_ranges(
    comp: &[u16],
    doc: &[u16],
    ranges: &[TsfRange],
) -> Vec<PreeditSpan> {
    if comp != doc {
        return Vec::new();
    }
    let to_char = |u: usize| {
        comp[..u]
            .iter()
            .filter(|&&c| !(0xDC00..=0xDFFF).contains(&c))
            .count()
    };
    ranges
        .iter()
        .filter_map(|r| {
            let a = usize::try_from(r.start).ok()?.min(comp.len());
            let b = usize::try_from(r.start.checked_add(r.len)?)
                .ok()?
                .min(comp.len());
            let (a, b) = (to_char(a), to_char(b));
            (b > a).then_some(PreeditSpan {
                range: (a, b),
                ..r.style
            })
        })
        .collect()
}

/// 本线程用到的 TSF 对象（克隆只是给 COM 接口加引用）。建一次用到线程结束；不在线程退出时释放——那时 COM 可能已经
/// 拆了，释放反而会崩。
#[derive(Clone)]
struct Tsf {
    tm: ITfThreadMgr,
    cat: ITfCategoryMgr,
    dam: ITfDisplayAttributeMgr,
    /// `Activate` 得到的客户端 id。等确认线程上已有 TSF 焦点上下文（CUAS 在跑）才激活，
    /// 免得在纯 IMM 输入法下由本库把 TSF 拉起来。
    tid: Option<u32>,
}

enum Slot {
    Untried,
    Ready(ManuallyDrop<Tsf>),
    /// COM 起不来：本线程不再重试，免得每次按键都白跑一遍。
    Unavailable,
}

thread_local! {
    static TSF: RefCell<Slot> = const { RefCell::new(Slot::Untried) };
}

unsafe fn create() -> Result<Tsf> {
    // 本库别处只在拖放时才初始化 COM，这里自己保证。已初始化（S_FALSE）或模式不同
    // （RPC_E_CHANGED_MODE，宿主应用自己初始化过）都能接着用，故不看返回值，也不配对反初始化。
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    Ok(Tsf {
        // 同一线程上拿到的是 CUAS 已经建好的那个线程管理器。
        tm: CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER)?,
        cat: CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?,
        dam: CoCreateInstance(&CLSID_TF_DisplayAttributeMgr, None, CLSCTX_INPROC_SERVER)?,
        tid: None,
    })
}

/// 读焦点上下文里组合串的显示属性。`comp` 是 IMM 给的组合串（UTF-16）。
pub(super) unsafe fn display_spans(comp: &[u16]) -> Vec<PreeditSpan> {
    // 只在短借用里取出 / 存回对象，TSF 调用一律在借用之外做：COM 调用可能泵消息，
    // 重入到另一条 `WM_IME_COMPOSITION` 时再借一次就会 panic。
    let cached = TSF.with(|slot| match &*slot.borrow() {
        Slot::Untried => Ok(None),
        Slot::Ready(t) => Ok(Some(Tsf::clone(t))),
        Slot::Unavailable => Err(()),
    });
    let mut t = match cached {
        Ok(Some(t)) => t,
        Ok(None) => match create() {
            Ok(t) => {
                TSF.with(|slot| *slot.borrow_mut() = Slot::Ready(ManuallyDrop::new(t.clone())));
                t
            }
            Err(e) => {
                log::debug!("ime: TSF 不可用，合成串用默认样式（{e}）");
                TSF.with(|slot| *slot.borrow_mut() = Slot::Unavailable);
                return Vec::new();
            }
        },
        Err(()) => return Vec::new(),
    };
    let had_tid = t.tid.is_some();
    let r = read(&mut t, comp);
    if !had_tid && t.tid.is_some() {
        TSF.with(|slot| {
            if let Slot::Ready(cached) = &mut *slot.borrow_mut() {
                cached.tid = t.tid;
            }
        });
    }
    r.unwrap_or_else(|e| {
        log::trace!("ime: 读 TSF 显示属性失败 {e}");
        Vec::new()
    })
}

unsafe fn read(t: &mut Tsf, comp: &[u16]) -> Result<Vec<PreeditSpan>> {
    // 先看有没有焦点上下文：纯 IMM 输入法下没有，到此为止，不去 `Activate`。
    let ctx = t.tm.GetFocus()?.GetTop()?;
    let tid = match t.tid {
        Some(tid) => tid,
        None => *t.tid.insert(t.tm.Activate()?),
    };
    let out = Rc::new(RefCell::new((Vec::new(), Vec::new())));
    let session: ITfEditSession = Reader {
        ctx: ctx.clone(),
        cat: t.cat.clone(),
        dam: t.dam.clone(),
        cap: comp.len(),
        out: out.clone(),
    }
    .into();
    // 同步只读会话：返回时回调已经跑完。上下文被锁着时 hr 是失败码，`out` 保持空。
    let hr = ctx.RequestEditSession(tid, &session, TF_ES_READ | TF_ES_SYNC)?;
    hr.ok()?;
    let (doc, ranges) = &*out.borrow();
    Ok(spans_from_ranges(comp, doc, ranges))
}

#[implement(ITfEditSession)]
struct Reader {
    ctx: ITfContext,
    cat: ITfCategoryMgr,
    dam: ITfDisplayAttributeMgr,
    /// IMM 组合串的长度（码元）。缓冲多留一个：上下文比组合串长时读出来也对不上，照样判不匹配。
    cap: usize,
    /// （上下文全文，逐段属性）。
    out: Rc<RefCell<(Vec<u16>, Vec<TsfRange>)>>,
}

impl ITfEditSession_Impl for Reader_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        // SAFETY: 都是本线程上的 TSF 调用，`ec` 在本回调内有效。
        unsafe {
            let all = self.ctx.GetStart(ec)?;
            all.ShiftEndToRange(ec, &self.ctx.GetEnd(ec)?, TF_ANCHOR_END)?;
            let mut doc = vec![0u16; self.cap + 1];
            let mut n = 0u32;
            all.GetText(ec, 0, &mut doc, &mut n)?;
            doc.truncate(n as usize);

            let prop = self.ctx.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            let mut ranges = Vec::new();
            let mut en = None;
            prop.EnumRanges(ec, &mut en, None)?;
            if let Some(en) = en {
                loop {
                    let mut r = [None];
                    let mut got = 0u32;
                    if en.Next(&mut r, &mut got).is_err() || got == 0 {
                        break;
                    }
                    let Some(r) = r[0].take() else { break };
                    // 单段读不出（属性 GUID 没人认领等）只丢这一段，其余照用。
                    let Some(style) = range_style(&prop, &self.cat, &self.dam, ec, &r) else {
                        continue;
                    };
                    let (mut start, mut len) = (0i32, 0i32);
                    let extent = r
                        .cast::<ITfRangeACP>()
                        .and_then(|acp| acp.GetExtent(&mut start, &mut len));
                    if extent.is_ok() {
                        ranges.push(TsfRange { start, len, style });
                    }
                }
            }
            *self.out.borrow_mut() = (doc, ranges);
        }
        Ok(())
    }
}

/// 一段的显示属性：属性值是 GUID 原子 → GUID → 显示属性信息。
unsafe fn range_style(
    prop: &ITfProperty,
    cat: &ITfCategoryMgr,
    dam: &ITfDisplayAttributeMgr,
    ec: u32,
    r: &ITfRange,
) -> Option<PreeditSpan> {
    let v = prop.GetValue(ec, r).ok()?;
    if v.Anonymous.Anonymous.vt != VT_I4 {
        return None;
    }
    let atom = v.Anonymous.Anonymous.Anonymous.lVal as u32;
    let mut info = None;
    let mut owner = GUID::zeroed();
    dam.GetDisplayAttributeInfo(&cat.GetGUID(atom).ok()?, &mut info, &mut owner)
        .ok()?;
    let mut da = TF_DISPLAYATTRIBUTE::default();
    info?.GetAttributeInfo(&mut da).ok()?;
    Some(span_style(&da))
}

fn span_style(da: &TF_DISPLAYATTRIBUTE) -> PreeditSpan {
    let underline = match da.lsStyle {
        TF_LS_NONE => UnderlineStyle::None,
        TF_LS_SOLID => UnderlineStyle::Solid,
        TF_LS_DOT => UnderlineStyle::Dot,
        TF_LS_DASH => UnderlineStyle::Dash,
        TF_LS_SQUIGGLE => UnderlineStyle::Squiggle,
        _ => UnderlineStyle::Solid,
    };
    PreeditSpan {
        range: (0, 0),
        underline,
        bold: da.fBoldLine.as_bool(),
        line: color(&da.crLine),
        fg: color(&da.crText),
        bg: color(&da.crBk),
    }
}

/// `TF_CT_NONE` = 输入法没指定，交给控件的默认色。
fn color(c: &TF_DA_COLOR) -> Option<Color> {
    // SAFETY: 联合体按 `type` 取对应的成员。
    let cr = unsafe {
        match c.r#type {
            TF_CT_SYSCOLOR => GetSysColor(SYS_COLOR_INDEX(c.Anonymous.nIndex)),
            TF_CT_COLORREF => c.Anonymous.cr.0,
            _ => return None,
        }
    };
    // COLORREF 是 0x00BBGGRR。
    Some(Color::rgb(cr as u8, (cr >> 8) as u8, (cr >> 16) as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16s(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn range(start: i32, len: i32, underline: UnderlineStyle) -> TsfRange {
        TsfRange {
            start,
            len,
            style: PreeditSpan {
                underline,
                ..PreeditSpan::default()
            },
        }
    }

    #[test]
    fn utf16_ranges_become_char_ranges() {
        // 「𠀀」占两个码元：码元 1..3 是一个字。
        let comp = u16s("a𠀀bc");
        let got = spans_from_ranges(
            &comp,
            &comp,
            &[
                range(0, 3, UnderlineStyle::Dot),
                range(3, 9, UnderlineStyle::Solid),
            ],
        );
        assert_eq!(got.len(), 2);
        assert_eq!(
            (got[0].range, got[0].underline),
            ((0, 2), UnderlineStyle::Dot)
        );
        assert_eq!(got[1].range, (2, 4), "越界的段裁到串尾");
    }

    #[test]
    fn mismatched_context_text_is_ignored() {
        let ranges = [range(0, 2, UnderlineStyle::Dot)];
        assert!(spans_from_ranges(&u16s("ab"), &u16s("abc"), &ranges).is_empty());
        assert!(
            spans_from_ranges(
                &u16s("ab"),
                &u16s("ab"),
                &[range(1, 0, UnderlineStyle::Dot)]
            )
            .is_empty(),
            "空段丢掉"
        );
    }

    #[test]
    fn colorref_is_bgr() {
        let mut c = TF_DA_COLOR {
            r#type: TF_CT_COLORREF,
            ..Default::default()
        };
        c.Anonymous.cr = windows::Win32::Foundation::COLORREF(0x0000_80ff);
        assert_eq!(color(&c), Some(Color::rgb(0xff, 0x80, 0)));
        assert_eq!(
            color(&TF_DA_COLOR::default()),
            None,
            "TF_CT_NONE 交给默认色"
        );
    }
}
