//! 输入法：`zwp_text_input_v3` 的纯逻辑部分（有单测），协议对象的收发在 `ime.rs`。
//!
//! # 协议要点
//!
//! - 文本焦点跟键盘焦点走：合成器发 `enter(surface)` / `leave`；焦点落在可编辑控件上时客户端
//!   `enable` + 状态请求 + `commit`，离开可编辑控件时 `disable` + `commit`。`enter` 之后与
//!   `disable` 之后，之前送出的状态全部作废，要重发。
//! - 状态请求（`set_content_type` / `set_cursor_rectangle` / `set_surrounding_text`）都是
//!   双缓冲的，`commit` 时生效。合成器数我们的 `commit` 次数作为 `done` 的 serial。
//! - 输入法发来的 `preedit_string` / `commit_string` / `delete_surrounding_text` 也是双缓冲，
//!   到 `done` 才按规定顺序一次性应用：清旧合成串 → 删周围文本 → 插入提交串 → 设新合成串。
//! - `done` 的 serial 与我们的 `commit` 次数不符：文本照常应用，但**不改**状态，积压的状态
//!   请求等到 serial 对上的 `done` 再发（协议原文）。
//!
//! 文本里的下标、长度都是 UTF-8 **字节**；框架的合成串与选区用**字符**，换算在这里做。

use crate::event::Preedit;

/// 协议规定 `set_surrounding_text` 的文本不超过 4000 字节。
pub(super) const SURROUNDING_MAX: usize = 4000;

/// 希望输入法看到的状态（`None` = 焦点不在可编辑控件上，应 `disable`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Want {
    /// 光标矩形（表面逻辑坐标）：x, y, w, h。
    pub rect: (i32, i32, i32, i32),
    /// 周围文本与光标 / 锚点（字节）。`None` = 不知道（密码框、非文本控件），不发。
    pub surrounding: Option<(String, u32, u32)>,
}

/// 要发的协议请求，按顺序。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Req {
    Enable,
    Disable,
    /// 内容类型：目前一律「普通文本」（框架没有向平台层暴露多行 / 密码属性）。
    ContentType,
    Rect(i32, i32, i32, i32),
    Surrounding(String, i32, i32),
    Commit,
}

/// `done` 时要应用的一批改动（顺序见模块说明）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Batch {
    /// 删掉光标前 / 后多少个**字符**（已按上次送出的周围文本从字节换算）。
    pub delete: (usize, usize),
    pub commit: Option<String>,
    /// 新合成串（空 = 没有合成）。
    pub preedit: Preedit,
}

#[derive(Default)]
pub(super) struct TextInputState {
    /// 当前有文本焦点（收到了 `enter`、还没 `leave`）。
    entered: bool,
    /// 已送出的 `commit` 次数。
    commits: u32,
    /// 最近一次 `done` 的 serial 与 `commits` 不符：状态请求先压着。
    stale: bool,
    /// 已随 `commit` 送出、合成器那边生效的状态。`enabled == false` 时其余无意义。
    enabled: bool,
    rect: Option<(i32, i32, i32, i32)>,
    surrounding: Option<(String, u32, u32)>,
    // ── 输入法发来、等 `done` 的 ──
    preedit: Option<(String, i32, i32)>,
    commit: Option<String>,
    delete: (u32, u32),
}

impl TextInputState {
    pub fn enabled(&self) -> bool {
        self.entered && self.enabled
    }

    /// 文本焦点进来：之前的状态作废。
    pub fn on_enter(&mut self) {
        self.entered = true;
        self.forget_sent();
    }

    /// 文本焦点离开：状态作废，没 `done` 的输入法改动丢弃。调用方负责清掉本地合成串。
    pub fn on_leave(&mut self) {
        self.entered = false;
        self.forget_sent();
        self.preedit = None;
        self.commit = None;
        self.delete = (0, 0);
    }

    fn forget_sent(&mut self) {
        self.enabled = false;
        self.rect = None;
        self.surrounding = None;
        self.stale = false;
    }

    pub fn on_preedit(&mut self, text: Option<String>, begin: i32, end: i32) {
        self.preedit = Some((text.unwrap_or_default(), begin, end));
    }

    pub fn on_commit_string(&mut self, text: Option<String>) {
        self.commit = text;
    }

    pub fn on_delete(&mut self, before: u32, after: u32) {
        self.delete = (before, after);
    }

    /// `done`：取出这一批改动，并按 serial 对账。
    pub fn on_done(&mut self, serial: u32) -> Batch {
        self.stale = serial != self.commits;
        let (before, after) = std::mem::take(&mut self.delete);
        // 删周围文本按「上次送出的周围文本」换算字节 → 字符（协议的长度相对于它）。没送过
        // （密码框等）就按一字节一字符近似——输入法没有上下文时本就极少发删除。
        let delete = match &self.surrounding {
            Some((text, cursor, _)) => (
                chars_before(text, *cursor as usize, before as usize),
                chars_after(text, *cursor as usize, after as usize),
            ),
            None => (before as usize, after as usize),
        };
        let preedit = match self.preedit.take() {
            Some((text, begin, end)) => preedit_from_bytes(text, begin, end),
            None => Preedit::default(),
        };
        Batch {
            delete,
            commit: self.commit.take().filter(|s| !s.is_empty()),
            preedit,
        }
    }

    /// 让合成器那边的状态跟上 `want`，返回要发的请求（空 = 什么都不用发）。
    pub fn sync(&mut self, want: Option<&Want>) -> Vec<Req> {
        if !self.entered {
            return Vec::new();
        }
        let mut out = Vec::new();
        match want {
            None => {
                if self.enabled {
                    out.push(Req::Disable);
                    self.forget_sent();
                }
            }
            Some(w) if !self.enabled => {
                // enable 重置一切状态：把全部状态随它一起送。
                out.extend([Req::Enable, Req::ContentType]);
                self.enabled = true;
                self.stale = false;
                self.push_state(w, &mut out, true);
            }
            // serial 没对上：状态请求等下一个对得上的 `done`。
            Some(_) if self.stale => {}
            Some(w) => self.push_state(w, &mut out, false),
        }
        if !out.is_empty() {
            out.push(Req::Commit);
            self.commits = self.commits.wrapping_add(1);
        }
        out
    }

    /// 放弃进行中的合成：`disable` 再 `enable`，输入法随之丢掉它手里的合成串。
    pub fn reset(&mut self, want: &Want) -> Vec<Req> {
        if !self.enabled() {
            return Vec::new();
        }
        let mut out = self.sync(None);
        out.extend(self.sync(Some(want)));
        out
    }

    fn push_state(&mut self, w: &Want, out: &mut Vec<Req>, all: bool) {
        if all || self.rect != Some(w.rect) {
            let (x, y, ww, h) = w.rect;
            out.push(Req::Rect(x, y, ww, h));
            self.rect = Some(w.rect);
        }
        if (all || self.surrounding != w.surrounding) && w.surrounding.is_some() {
            if let Some((t, c, a)) = &w.surrounding {
                out.push(Req::Surrounding(t.clone(), *c as i32, *a as i32));
            }
        }
        self.surrounding = w.surrounding.clone();
    }
}

/// 输入法的合成串（字节下标的光标）→ 框架的 `Preedit`（字符下标）。两端都是 -1 表示隐藏光标，
/// 这里把光标放在末尾；两端不同时那一段作为「选中分句」高亮。
fn preedit_from_bytes(text: String, begin: i32, end: i32) -> Preedit {
    let len = text.chars().count();
    let to_char = |b: i32| -> Option<usize> {
        let b = usize::try_from(b).ok()?;
        text.is_char_boundary(b).then(|| text[..b].chars().count())
    };
    let (b, e) = (to_char(begin), to_char(end));
    let caret = b.unwrap_or(len).min(len);
    let sel = match (b, e) {
        (Some(b), Some(e)) if b != e => Some((b.min(e), b.max(e))),
        _ => None,
    };
    Preedit { text, caret, sel }
}

/// 从字节下标 `cursor` 往前数 `bytes` 个字节，含多少个字符（落在字符中间的按整字符算）。
fn chars_before(text: &str, cursor: usize, bytes: usize) -> usize {
    let cursor = cursor.min(text.len());
    let start = cursor.saturating_sub(bytes);
    text.char_indices()
        .filter(|&(i, _)| i >= start && i < cursor)
        .count()
}

/// 从字节下标 `cursor` 往后数 `bytes` 个字节，含多少个字符。
fn chars_after(text: &str, cursor: usize, bytes: usize) -> usize {
    let cursor = cursor.min(text.len());
    let end = cursor.saturating_add(bytes);
    text.char_indices()
        .filter(|&(i, _)| i >= cursor && i < end)
        .count()
}

/// 框架给的正文与选区（字符下标）→ 协议的周围文本与光标 / 锚点（字节）。超过
/// [`SURROUNDING_MAX`] 时截取光标附近的一段（前后各约一半，落在字符边界上）。
pub(super) fn surrounding(text: &str, sel: (usize, usize)) -> (String, u32, u32) {
    let byte_of = |c: usize| text.char_indices().nth(c).map_or(text.len(), |(i, _)| i);
    // 选区的 start 是锚点、end 是光标（框架只给有序范围，方向无从得知；输入法只关心范围）。
    let (anchor, cursor) = (byte_of(sel.0), byte_of(sel.1));
    if text.len() <= SURROUNDING_MAX {
        return (text.to_string(), cursor as u32, anchor as u32);
    }
    let half = SURROUNDING_MAX / 2;
    let mut start = anchor.min(cursor).saturating_sub(half);
    let mut end = (start + SURROUNDING_MAX).min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let clip = |b: usize| (b.clamp(start, end) - start) as u32;
    (text[start..end].to_string(), clip(cursor), clip(anchor))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn want(x: i32) -> Want {
        Want {
            rect: (x, 10, 1, 20),
            surrounding: Some(("ab".into(), 2, 2)),
        }
    }

    #[test]
    fn nothing_is_sent_before_enter() {
        let mut s = TextInputState::default();
        assert!(s.sync(Some(&want(1))).is_empty());
    }

    #[test]
    fn enable_sends_the_whole_state_once_then_only_changes() {
        let mut s = TextInputState::default();
        s.on_enter();
        assert_eq!(
            s.sync(Some(&want(1))),
            vec![
                Req::Enable,
                Req::ContentType,
                Req::Rect(1, 10, 1, 20),
                Req::Surrounding("ab".into(), 2, 2),
                Req::Commit
            ]
        );
        assert!(s.sync(Some(&want(1))).is_empty(), "没变化：不再 commit");
        assert_eq!(
            s.sync(Some(&want(5))),
            vec![Req::Rect(5, 10, 1, 20), Req::Commit],
            "只有光标动了"
        );
    }

    #[test]
    fn disable_is_sent_once_and_leaving_editable_focus_then_back_reenables() {
        let mut s = TextInputState::default();
        s.on_enter();
        s.sync(Some(&want(1)));
        assert_eq!(s.sync(None), vec![Req::Disable, Req::Commit]);
        assert!(s.sync(None).is_empty(), "disable 去重");
        assert_eq!(s.sync(Some(&want(1)))[0], Req::Enable);
    }

    #[test]
    fn enter_invalidates_what_was_sent() {
        let mut s = TextInputState::default();
        s.on_enter();
        s.sync(Some(&want(1)));
        s.on_leave();
        assert!(
            s.sync(Some(&want(1))).is_empty(),
            "leave 之后请求会被忽略，不发"
        );
        s.on_enter();
        assert_eq!(
            s.sync(Some(&want(1)))[0],
            Req::Enable,
            "enter 之后要重新 enable"
        );
        // 换到本应用的另一个表面：合成器也会发 enter，之前的状态同样作废。
        s.on_enter();
        assert_eq!(s.sync(Some(&want(1)))[0], Req::Enable);
    }

    #[test]
    fn done_applies_the_batch_in_protocol_order_and_resets_it() {
        let mut s = TextInputState::default();
        s.on_enter();
        s.sync(Some(&Want {
            rect: (0, 0, 1, 1),
            surrounding: Some(("你好ab".into(), 6, 6)),
        }));
        s.on_delete(6, 1);
        s.on_commit_string(Some("世界".into()));
        s.on_preedit(Some("zhong".into()), 5, 5);
        let b = s.on_done(1);
        assert_eq!(b.delete, (2, 1), "6 字节 = 两个汉字；后面 1 字节 = a");
        assert_eq!(b.commit.as_deref(), Some("世界"));
        assert_eq!(
            b.preedit,
            Preedit {
                text: "zhong".into(),
                caret: 5,
                sel: None
            }
        );
        assert_eq!(s.on_done(1), Batch::default(), "done 之后回到初值");
    }

    #[test]
    fn mismatched_serial_still_applies_text_but_holds_state_requests() {
        let mut s = TextInputState::default();
        s.on_enter();
        s.sync(Some(&want(1))); // commits = 1
        s.on_commit_string(Some("x".into()));
        let b = s.on_done(0);
        assert_eq!(b.commit.as_deref(), Some("x"), "文本照常应用");
        assert!(s.sync(Some(&want(9))).is_empty(), "状态请求压着");
        s.on_done(1);
        assert_eq!(
            s.sync(Some(&want(9))),
            vec![Req::Rect(9, 10, 1, 20), Req::Commit],
            "serial 对上后补发"
        );
        assert!(!s.sync(None).is_empty(), "disable 不受 serial 限制");
    }

    #[test]
    fn serial_counts_every_commit() {
        let mut s = TextInputState::default();
        s.on_enter();
        s.sync(Some(&want(1)));
        s.sync(Some(&want(2)));
        s.sync(Some(&want(3)));
        assert_eq!(s.on_done(3), Batch::default());
        assert!(!s.stale);
    }

    #[test]
    fn preedit_cursor_bytes_become_chars_and_hidden_cursor_goes_to_end() {
        let p = preedit_from_bytes("中文ab".into(), 3, 3);
        assert_eq!((p.caret, p.sel), (1, None));
        let p = preedit_from_bytes("中文ab".into(), -1, -1);
        assert_eq!(p.caret, 4);
        let p = preedit_from_bytes("中文ab".into(), 0, 6);
        assert_eq!((p.caret, p.sel), (0, Some((0, 2))), "两端不同 = 高亮分句");
        let p = preedit_from_bytes("中文".into(), 1, 1);
        assert_eq!(p.caret, 2, "落在字符中间的非法下标：光标放末尾");
    }

    #[test]
    fn reset_disables_then_reenables_only_when_enabled() {
        let mut s = TextInputState::default();
        assert!(s.reset(&want(1)).is_empty());
        s.on_enter();
        s.sync(Some(&want(1)));
        let r = s.reset(&want(1));
        assert_eq!(r[..2], [Req::Disable, Req::Commit]);
        assert_eq!(r[2], Req::Enable);
        assert_eq!(r.last(), Some(&Req::Commit));
    }

    #[test]
    fn surrounding_converts_char_selection_to_bytes() {
        assert_eq!(surrounding("你好ab", (2, 3)), ("你好ab".into(), 7, 6));
        assert_eq!(surrounding("", (0, 0)), (String::new(), 0, 0));
    }

    #[test]
    fn long_surrounding_is_clipped_around_the_cursor_on_char_boundaries() {
        let text: String = "字".repeat(3000); // 9000 字节
        let (t, c, a) = surrounding(&text, (2000, 2000));
        assert!(t.len() <= SURROUNDING_MAX);
        assert!(t.chars().all(|ch| ch == '字'), "没有切坏字符");
        assert_eq!(c, a);
        assert!(t.is_char_boundary(c as usize));
        assert!(
            (c as usize) > 1000 && (c as usize) < 3000,
            "光标在截取段中部附近：{c}"
        );
    }

    #[test]
    fn delete_lengths_count_whole_chars() {
        assert_eq!(chars_before("a你b", 4, 3), 1);
        assert_eq!(chars_before("a你b", 4, 4), 2);
        assert_eq!(chars_after("a你b", 1, 3), 1);
        assert_eq!(chars_after("a你b", 1, 100), 2);
    }
}
