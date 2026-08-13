//! 控件文案载体 [`TextContent`]：一段静态串，或一个绑定的 `Signal<String>`。
//!
//! 凡是接受"一段文案"的控件参数（`label`/`button`/`link`/`badge`/`checkbox` …）都收
//! `impl Into<TextContent>`，于是 `&str`、`String` 与 `Signal<String>` 可以互换地传进去：
//!
//! ```
//! use windui::prelude::*;
//! let caption = signal(String::from("暂停"));
//! let fixed = Element::button("保存");        // 静态文案
//! let dynamic = Element::button(caption);     // 跟随信号，点击时 caption.set(..) 即换字
//! ```
//!
//! # 为什么是"参数类型泛化"，而不是每个控件再加一个 `_signal` 构造器
//!
//! 本库先前只有 `Element::label_signal` 一条动态文案路径，它背后是一个与 `Label` 并列的
//! `DynLabel` widget——两者的换行、`max_lines` 裁剪、截断算法逐行重复了一遍，改一处得改
//! 两处。把这条路子推广到 `button`/`link` 意味着再复制两份三态动画与绘制代码；更要命的是
//! `.intent()`/`.outline()`/`.url()` 这类修饰符靠 `downcast_mut::<Button>()` 分派，孪生
//! widget 类型会让每个修饰符都得多试一次 downcast。
//!
//! 于是动态性下沉成**字段的类型**而不是**控件的类型**：`Button` 只有一个，它的 `label`
//! 字段能装静态串也能装信号。对外的规则因此收敛成一句话——**凡是接受文案的参数都可以传
//! `Signal<String>`**，不必记哪个控件有没有 `_signal` 孪生构造器。`_signal` 后缀继续留给
//! 参数类型确实不同的场景（`list_signal` 收 `Signal<Vec<T>>`、`dropdown_signal` 收两个
//! 信号），二者的分工是清楚的。
//!
//! 另一个候选是加个修饰符 `.text_signal(sig)`。它落到不支持的控件上只能 `debug_assert`
//! 在运行期报错，而参数类型泛化的误用在**编译期**就过不去；且 `button("占位").text_signal(s)`
//! 里的 `"占位"` 是个永远不会显示的死参数。

use std::borrow::Cow;

use crate::signal::Signal;

/// 控件文案：静态串或信号绑定。
///
/// 通过 `impl Into<TextContent>` 出现在控件构造器的签名里，调用方一般不直接写这个类型：
/// 传 `&str`/`String` 得到 [`TextContent::Static`]，传 `Signal<String>` 得到
/// [`TextContent::Bound`]。
///
/// 绑定值在每次 `measure`/`paint` 时现取（见 [`TextContent::resolve`]），因此**没有**
/// "写进去的值和显示的值不同步"这一类状态：控件不缓存文案副本。
///
/// # 误用在编译期拦下
///
/// 本库的修饰符误用（`.url()` 链到按钮上之类）只能靠 `#[track_caller]` + `debug_assert`
/// 在运行期喊停。文案绑定不走那条路：它是**参数类型**，绑错东西根本编译不过。
///
/// ```compile_fail,E0277
/// use windui::prelude::*;
/// let count = signal(0i32);
/// let _ = Element::button(count); // Signal<i32> 不是文案：没有 Into<TextContent>
/// ```
///
/// 要显示一个数字，把它格式化进文案信号（或另建一个 `Signal<String>` 派生）：
///
/// ```
/// use windui::prelude::*;
/// let count = signal(0i32);
/// let caption = signal(format!("已选 {} 项", count.get()));
/// let _ = Element::button(caption).on_click(move |_| {
///     count.set(count.get() + 1);
///     caption.set(format!("已选 {} 项", count.get()));
/// });
/// ```
pub enum TextContent {
    /// 构建期定下、此后不变的文案。
    Static(String),
    /// 绑定到信号：每帧现取当前值，`Signal::set`/`update` 即改文案。
    Bound(Signal<String>),
}

impl TextContent {
    /// 取当前文案。静态串零拷贝借出；绑定值克隆一份出来——信号的存储在
    /// `RefCell` 保护的线程局部运行时里，借着它跨越整个绘制过程会把一次
    /// 不相干的 `Signal::set` 变成 panic，代价换的是这份安全。
    pub fn resolve(&self) -> Cow<'_, str> {
        match self {
            TextContent::Static(s) => Cow::Borrowed(s.as_str()),
            TextContent::Bound(sig) => Cow::Owned(sig.get()),
        }
    }
}

impl From<String> for TextContent {
    fn from(s: String) -> Self {
        TextContent::Static(s)
    }
}

impl From<&str> for TextContent {
    fn from(s: &str) -> Self {
        TextContent::Static(s.to_string())
    }
}

impl From<&String> for TextContent {
    fn from(s: &String) -> Self {
        TextContent::Static(s.clone())
    }
}

impl From<Cow<'_, str>> for TextContent {
    fn from(s: Cow<'_, str>) -> Self {
        TextContent::Static(s.into_owned())
    }
}

impl From<Signal<String>> for TextContent {
    fn from(sig: Signal<String>) -> Self {
        TextContent::Bound(sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::signal;

    #[test]
    fn static_resolves_without_clone() {
        let t = TextContent::from("保存");
        assert_eq!(t.resolve(), "保存");
        assert!(
            matches!(t.resolve(), Cow::Borrowed(_)),
            "静态串应零拷贝借出"
        );
    }

    #[test]
    fn bound_follows_signal() {
        let s = signal(String::from("暂停"));
        let t = TextContent::from(s);
        assert_eq!(t.resolve(), "暂停");
        s.set(String::from("播放"));
        assert_eq!(t.resolve(), "播放", "绑定值应现取，不缓存副本");
    }

    #[test]
    fn accepts_all_string_shapes() {
        assert_eq!(TextContent::from(String::from("a")).resolve(), "a");
        assert_eq!(TextContent::from(&String::from("b")).resolve(), "b");
        assert_eq!(TextContent::from(Cow::Borrowed("c")).resolve(), "c");
    }
}
