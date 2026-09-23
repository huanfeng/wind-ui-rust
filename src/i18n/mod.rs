//! 多语言（i18n）：译文目录、消息格式化与运行期换语言。
//!
//! 设计全文见 `docs/i18n-design.md`。三分钟版本（`ignore` 是因为 `include_str!` 的路径
//! 相对源文件解析，doctest 里取不到；这套 API 的编译核验在 `examples/i18n.rs`）：
//!
//! ```ignore
//! use windui::prelude::*;
//!
//! let locales = Locales::builder()
//!     .embed(include_str!("../i18n/zh-CN.toml"))   // 内置：编译进二进制
//!     .embed(include_str!("../i18n/en.toml"))
//!     .load_dir("i18n")                            // 可选：外部覆盖，按 key 压过内置
//!     .fallback("en")
//!     .initial(Initial::System)                    // 默认是 Fixed("zh-CN")
//!     .build();
//!
//! let mut app = App::new("Demo", 400, 300).locales(locales);
//! let lang = app.locale_handle();
//! let ui = Element::col()
//!     .child(Element::button(t!("menu.copy")))                 // 跟随换语言
//!     .child(Element::label(t!("file.selected", count = 3)))   // 位置格式化 + 复数
//!     .child(Element::button("English").on_click(move |_| { lang.set("en"); }));
//! # let _ = ui;
//! ```
//!
//! # 两个宏的分工（**背下来，编译期拦不住**）
//!
//! - [`t!`](crate::t) 产出 [`Message`]，进控件（任何收 `impl Into<TextContent>` 的参数）。
//!   参数存着不求值，**换语言自动跟随**。
//! - [`tr!`](crate::tr) 产出 `String`，给菜单项 / 日志 / `ctx.toast` 这类**需要 `String`**
//!   的地方。它当场定格，**换语言不跟随**。
//!
//! 把 `tr!` 塞进 `Element::label` 能编过（`String` 有 `Into<TextContent>`），症状只是那条
//! 文案换语言时不变。[`lint::check`] 查不出这个（它只看译文文件，不看调用点），靠 review。
//!
//! # 与 [`ThemeHandle`](crate::app::ThemeHandle) 的差异：这里没有"每帧同步"
//!
//! 主题有 `theme_src`（句柄）+ `theme::current()`（线程局部）两份，宿主每帧从前者刷后者，
//! 因为主题**可能**将来按窗口分。语言不会——它是应用级的，且所有窗口同线程（见
//! 「多窗口地基」）。于是这里**只有一个线程局部**，[`LocaleHandle`] 是它的薄句柄，
//! 不进 `UiHost`、不需要每帧同步，也就少了一处"忘了注入"的失效点。
//!
//! 宿主只做一件事：每帧比对 [`current()`] 的指针，变了就整窗重排（`UiHost::begin_frame`）。
//! 不能只靠 `set` 里的 `anim::request_repaint`——控件回调里发出的请求会被下一帧开头的
//! `reset_request` 清掉，那一帧只局部重画被点的按钮。

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub mod format;
pub mod lint;
pub mod parse;
pub mod plural;

pub use format::Pattern;
pub use parse::{LangFile, Meta, ParseError};
pub use plural::Category;

/// 框架自带文案。下游可用同名 key（`windui.*`）覆盖，见 [`LocalesBuilder::build`]。
const BUILTIN: &[&str] = &[
    include_str!("../../i18n/zh-CN.toml"),
    include_str!("../../i18n/en.toml"),
];

/// 一条 key 对应的值。
#[derive(Debug, Clone)]
pub enum Entry {
    /// 普通值。
    One(Pattern),
    /// 变体表：复数类别（`one`/`other`…）或 select 变体（`male`/`female`/`other`）。
    Variants(BTreeMap<Box<str>, Pattern>),
    /// 字符串数组（星期、月份这类有序列表）。
    List(Vec<Pattern>),
}

/// 格式化参数的值。
///
/// 信号变体的存在让「动态数字 + 多语言」是一条路而不是两条：`Signal` 是 `Copy` 句柄
/// （见 [`Signal`](crate::signal::Signal)），装进 [`Message`] 不增负担，显示时现取。
///
/// 它同时消灭一类常见错误——把**已翻译的整句**塞进 `Signal<String>`（换语言不跟随），
/// 正确做法是只把**变量值**塞进去。
#[derive(Debug, Clone)]
pub enum ArgValue {
    Str(Rc<str>),
    Int(i64),
    Float(f64),
    SigStr(crate::signal::Signal<String>),
    SigInt(crate::signal::Signal<i64>),
}

impl ArgValue {
    fn render_into(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            ArgValue::Str(s) => out.push_str(s),
            ArgValue::Int(v) => {
                let _ = write!(out, "{v}");
            }
            ArgValue::Float(v) => {
                let _ = write!(out, "{v}");
            }
            ArgValue::SigStr(s) => out.push_str(&s.get()),
            ArgValue::SigInt(s) => {
                let _ = write!(out, "{}", s.get());
            }
        }
    }

    /// 当作复数轴的数量看。非数值参数返回 `None`。
    fn as_count(&self) -> Option<i64> {
        match self {
            ArgValue::Int(v) => Some(*v),
            ArgValue::Float(v) => Some(*v as i64),
            ArgValue::SigInt(s) => Some(s.get()),
            ArgValue::Str(_) | ArgValue::SigStr(_) => None,
        }
    }

    /// 当作 select / index 的字符串看。
    fn as_text(&self) -> String {
        let mut s = String::new();
        self.render_into(&mut s);
        s
    }
}

impl From<&str> for ArgValue {
    fn from(s: &str) -> Self {
        ArgValue::Str(s.into())
    }
}
impl From<&String> for ArgValue {
    fn from(s: &String) -> Self {
        ArgValue::Str(s.as_str().into())
    }
}
impl From<String> for ArgValue {
    fn from(s: String) -> Self {
        ArgValue::Str(s.into())
    }
}
impl From<Cow<'_, str>> for ArgValue {
    fn from(s: Cow<'_, str>) -> Self {
        ArgValue::Str(s.as_ref().into())
    }
}
impl From<char> for ArgValue {
    fn from(c: char) -> Self {
        ArgValue::Str(c.to_string().into())
    }
}
impl From<bool> for ArgValue {
    fn from(b: bool) -> Self {
        ArgValue::Str(if b { "true" } else { "false" }.into())
    }
}
macro_rules! arg_from_int {
    ($($t:ty),*) => { $(impl From<$t> for ArgValue {
        fn from(v: $t) -> Self { ArgValue::Int(v as i64) }
    })* };
}
arg_from_int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
impl From<f32> for ArgValue {
    fn from(v: f32) -> Self {
        ArgValue::Float(v as f64)
    }
}
impl From<f64> for ArgValue {
    fn from(v: f64) -> Self {
        ArgValue::Float(v)
    }
}
impl From<crate::signal::Signal<String>> for ArgValue {
    fn from(s: crate::signal::Signal<String>) -> Self {
        ArgValue::SigStr(s)
    }
}
impl From<crate::signal::Signal<i64>> for ArgValue {
    fn from(s: crate::signal::Signal<i64>) -> Self {
        ArgValue::SigInt(s)
    }
}

/// 待翻译的消息：key + 参数。**不含已翻译的文本**——那正是它能跟随换语言的原因。
///
/// 一般由 [`t!`](crate::t) 宏构造，不直接写这个类型。
#[derive(Debug, Clone)]
pub struct Message {
    key: Cow<'static, str>,
    named: Vec<(Cow<'static, str>, ArgValue)>,
    pos: Vec<ArgValue>,
}

/// 保留参数名：`count` 选复数类别，`select` 选变体，`index` 取数组下标。
/// 它们同时仍是普通占位符——`{count}` 照常能在译文里代入数字。
const ARG_COUNT: &str = "count";
const ARG_SELECT: &str = "select";
const ARG_INDEX: &str = "index";

impl Message {
    pub fn new(key: impl Into<Cow<'static, str>>) -> Self {
        Self {
            key: key.into(),
            named: Vec::new(),
            pos: Vec::new(),
        }
    }

    /// 加一个命名参数（`t!("k", name = v)` 展开到这里）。
    pub fn set(mut self, name: impl Into<Cow<'static, str>>, v: impl Into<ArgValue>) -> Self {
        self.named.push((name.into(), v.into()));
        self
    }

    /// 追加一个位置参数（`t!("k", a, b)` 展开到这里，对应 `{0}` `{1}`）。
    pub fn push(mut self, v: impl Into<ArgValue>) -> Self {
        self.pos.push(v.into());
        self
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    fn arg(&self, name: &str) -> Option<&ArgValue> {
        self.named
            .iter()
            .find(|(k, _)| k.as_ref() == name)
            .map(|(_, v)| v)
    }

    /// 按当前语言解析成最终文本。
    pub fn resolve(&self) -> String {
        current().format(self)
    }
}

impl From<Message> for String {
    fn from(m: Message) -> String {
        m.resolve()
    }
}

/// 一种语言的对外信息（供设置页填下拉）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleInfo {
    /// BCP-47 标签，`LocaleHandle::set` 收的就是它。
    pub id: String,
    /// 显示名（取自文件 `[meta] name`，缺省时退回 `id`）。
    pub name: String,
    /// 从右向左书写。**当前仅是标记，渲染层不消费**（见 `docs/i18n-design.md` §9）。
    pub rtl: bool,
}

/// 启动时选哪种语言。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Initial {
    /// 固定语言。**这是默认值（`zh-CN`）**。
    Fixed(String),
    /// 跟随系统偏好（[`platform::system_locales`](crate::platform::system_locales)）。
    ///
    /// 默认**不**开：默认跟随系统的话，现有下游一升级，装英文系统的用户右键菜单
    /// 会突然变英文——那是没人要求过的行为变更。要它就显式写出来。
    System,
}

impl Default for Initial {
    fn default() -> Self {
        Initial::Fixed("zh-CN".into())
    }
}

type Table = Rc<BTreeMap<String, Entry>>;

#[derive(Debug, Clone, Default)]
struct LangData {
    name: Option<String>,
    rtl: bool,
    fallback: Option<String>,
    /// 内置层（低→高）：框架自带在最低，应用 `embed` 的依次叠上。
    embedded: Vec<Table>,
    /// 外部层（低→高）：`load_dir` 扫出来的，`reload` 时整体重建。
    external: Vec<Table>,
}

/// 已加载的全部语言。由 [`Locales::builder`] 造，交给
/// [`App::locales`](crate::app::App::locales) 或 [`install`]。
#[derive(Debug, Clone)]
pub struct Locales {
    langs: BTreeMap<String, LangData>,
    fallback: String,
    initial: Initial,
    /// `load_dir` 记下的目录，供 [`LocaleHandle::reload`] 重扫。
    dirs: Vec<PathBuf>,
}

impl Default for Locales {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl Locales {
    pub fn builder() -> LocalesBuilder {
        LocalesBuilder::default()
    }

    /// 任何一种已加载的语言里有没有这条 key。
    ///
    /// 给 [`lint::check_usage`] 用：代码里 `t!("a.b")` 的 key 只要**某个**语言有译文就算
    /// 有着落（其余语言缺的那份由 [`lint::check`] 管），故这里是"任一"而不是"当前语言"。
    pub fn has_key(&self, key: &str) -> bool {
        self.langs.values().any(|d| {
            d.embedded
                .iter()
                .chain(d.external.iter())
                .any(|t| t.contains_key(key))
        })
    }

    /// 哪些语言**自己没有** `windui.*` 文案（框架自带的菜单文案会落到回退语言）。
    ///
    /// 这是"日文界面里冒出一份 Cut / Copy / Paste"的成因：回退链正按设计工作，只是框架
    /// 自带译文只有中英两份。库在 debug 下会为每一条喊一声 `log::warn!`；公开成方法是为了
    /// 让应用能在自己的测试里钉死这件事：
    ///
    /// ```
    /// # use windui::prelude::*;
    /// let locales = Locales::builder()
    ///     .embed("[meta]\nlocale = \"ja\"\n[app]\nhi = \"こんにちは\"\n")
    ///     .build();
    /// assert_eq!(locales.languages_missing_framework_strings(), vec!["ja".to_string()]);
    /// ```
    ///
    /// 补的办法是在该语言的译文里写上那批 key（清单见仓库 `i18n/zh-CN.toml`）。
    pub fn languages_missing_framework_strings(&self) -> Vec<String> {
        self.langs
            .iter()
            .filter(|(_, d)| {
                !d.embedded
                    .iter()
                    .chain(d.external.iter())
                    .any(|t| t.keys().any(|k| k.starts_with("windui.")))
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 已加载的语言（按 id 升序）。
    pub fn available(&self) -> Vec<LocaleInfo> {
        self.langs
            .iter()
            .map(|(id, d)| LocaleInfo {
                id: id.clone(),
                name: d.name.clone().unwrap_or_else(|| id.clone()),
                rtl: d.rtl,
            })
            .collect()
    }

    /// 把想要的语言标签匹配到一个已加载的语言。
    ///
    /// 三级宽松降级：精确（大小写不敏感）→ 逐级去掉末尾子标签（`zh-Hans-CN` → `zh-Hans`
    /// → `zh`）→ 同基础语言的任一变体（`zh` → `zh-CN`）。最后一级是必要的：系统报的是
    /// `zh-Hans-CN`，而译文文件通常叫 `zh-CN`，没有它就永远匹配不上。
    pub fn resolve_lang(&self, want: &str) -> Option<String> {
        let want_l = want.to_ascii_lowercase().replace('_', "-");
        if let Some(id) = self.langs.keys().find(|k| k.to_ascii_lowercase() == want_l) {
            return Some(id.clone());
        }
        let mut probe = want_l.as_str();
        while let Some(cut) = probe.rfind('-') {
            probe = &probe[..cut];
            if let Some(id) = self.langs.keys().find(|k| k.to_ascii_lowercase() == probe) {
                return Some(id.clone());
            }
        }
        let base = want_l.split('-').next().unwrap_or("");
        self.langs
            .keys()
            .find(|k| k.to_ascii_lowercase().split('-').next() == Some(base))
            .cloned()
    }

    /// 解析出某种语言的查找视图。`want` 匹配不上时用 fallback；再匹配不上则给空目录
    /// （此时所有 key 都 miss，表现见 [`Catalog::text`]）。
    pub fn catalog(&self, want: &str) -> Catalog {
        let lang = self
            .resolve_lang(want)
            .or_else(|| self.resolve_lang(&self.fallback));
        let mut merged: BTreeMap<String, Entry> = BTreeMap::new();
        let lang = match lang {
            Some(l) => l,
            None => {
                return Catalog {
                    lang: want.to_string(),
                    rtl: false,
                    entries: merged,
                }
            }
        };
        let data = &self.langs[&lang];
        // 回退语言先铺底，当前语言后盖上：同 key 后写的赢。
        let fb = data
            .fallback
            .clone()
            .unwrap_or_else(|| self.fallback.clone());
        if !fb.eq_ignore_ascii_case(&lang) {
            if let Some(fb_id) = self.resolve_lang(&fb) {
                if fb_id != lang {
                    merge_lang(&mut merged, &self.langs[&fb_id]);
                }
            }
        }
        merge_lang(&mut merged, data);
        Catalog {
            lang,
            rtl: data.rtl,
            entries: merged,
        }
    }

    fn initial_lang(&self) -> String {
        match &self.initial {
            Initial::Fixed(l) => l.clone(),
            Initial::System => {
                for want in crate::platform::system_locales() {
                    if let Some(id) = self.resolve_lang(&want) {
                        return id;
                    }
                }
                self.fallback.clone()
            }
        }
    }

    /// 重扫 `load_dir` 记下的目录，重建外部层。
    fn rescan(&mut self) {
        for d in self.langs.values_mut() {
            d.external.clear();
        }
        let dirs = self.dirs.clone();
        for dir in dirs {
            self.scan_dir(&dir);
        }
    }

    fn scan_dir(&mut self, dir: &Path) {
        let rd = match std::fs::read_dir(dir) {
            Ok(rd) => rd,
            Err(e) => {
                log::warn!("i18n: 读不了译文目录 {}：{e}", dir.display());
                return;
            }
        };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .is_some_and(|x| x.eq_ignore_ascii_case("toml"))
            })
            .collect();
        // 目录序在各平台不保证一致；排序让"同 key 谁压谁"可复现。
        files.sort();
        for path in files {
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("i18n: 读不了 {}：{e}", path.display());
                    continue;
                }
            };
            let id_hint = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            match parse::parse(&src) {
                Ok(f) => self.add_layer(f, Some(id_hint), false),
                Err(e) => {
                    // 单文件失败只丢那一个文件：用户改坏了 fr.toml，不该让整个应用
                    // （含内置译文）跟着没文案。
                    log::warn!("i18n: {} 解析失败，已跳过：{e}", path.display());
                }
            }
        }
    }

    /// 对**自己没有 `windui.*` 文案**的语言喊一声（仅 debug）。
    ///
    /// 这是一类只在界面上显形、不报任何错的问题：应用加了日文译文，框架自带的只有中英两份，
    /// 于是右键菜单按回退链落到英文——日文界面里冒出一份 Cut / Copy / Paste。它**不是**
    /// 故障（回退链正按设计工作），但多半也不是作者想要的，而唯一的症状是"截图看着怪"。
    ///
    /// 只在 debug 下喊：这是给开发者的提示，不是给用户的。补的办法是在该语言的译文里
    /// 写上 `windui.*` 这批 key（它们的清单见 `i18n/zh-CN.toml`）。
    fn warn_languages_without_framework_strings(&self) {
        if !cfg!(debug_assertions) {
            return;
        }
        for id in self.languages_missing_framework_strings() {
            log::warn!(
                "i18n: 语言 `{id}` 没有 `windui.*` 文案，框架自带的菜单（剪切/复制/…）\
                 会落到回退语言 `{}`。要让它们也是 `{id}`，在该语言的译文里补上这批 key\
                 （清单见仓库 i18n/zh-CN.toml）。",
                self.fallback
            );
        }
    }

    fn add_layer(&mut self, f: LangFile, id_hint: Option<String>, embedded: bool) {
        let id = match f.meta.locale.clone().or(id_hint) {
            Some(id) if !id.is_empty() => id,
            _ => {
                log::warn!("i18n: 译文缺 `[meta] locale` 且无法从文件名推断，已跳过");
                debug_assert!(false, "译文缺 [meta] locale");
                return;
            }
        };
        let d = self.langs.entry(id).or_default();
        if f.meta.name.is_some() {
            d.name = f.meta.name.clone();
        }
        if f.meta.fallback.is_some() {
            d.fallback = f.meta.fallback.clone();
        }
        d.rtl |= f.meta.rtl;
        let table = Rc::new(f.entries);
        if embedded {
            d.embedded.push(table);
        } else {
            d.external.push(table);
        }
    }
}

fn merge_lang(out: &mut BTreeMap<String, Entry>, d: &LangData) {
    for layer in d.embedded.iter().chain(d.external.iter()) {
        for (k, v) in layer.iter() {
            out.insert(k.clone(), v.clone());
        }
    }
}

/// [`Locales`] 的构建器。
#[derive(Debug, Default)]
pub struct LocalesBuilder {
    files: Vec<(LangFile, Option<String>)>,
    dirs: Vec<PathBuf>,
    fallback: Option<String>,
    initial: Option<Initial>,
}

impl LocalesBuilder {
    /// 内嵌一份译文（`include_str!`）。语言 id 取自文件的 `[meta] locale`。
    pub fn embed(mut self, src: &str) -> Self {
        match parse::parse(src) {
            Ok(f) => self.files.push((f, None)),
            Err(e) => {
                // 内嵌译文是开发者自己的文件，坏了就是 bug：debug 下当场炸，
                // release 下退化为"少一种语言"而不是崩在用户脸上。
                log::error!("i18n: 内嵌译文解析失败：{e}");
                debug_assert!(false, "内嵌译文解析失败：{e}");
            }
        }
        self
    }

    /// 同 [`embed`](Self::embed)，但显式指定语言 id（文件没写 `[meta] locale` 时用）。
    pub fn embed_as(mut self, id: impl Into<String>, src: &str) -> Self {
        match parse::parse(src) {
            Ok(f) => self.files.push((f, Some(id.into()))),
            Err(e) => {
                log::error!("i18n: 内嵌译文解析失败：{e}");
                debug_assert!(false, "内嵌译文解析失败：{e}");
            }
        }
        self
    }

    /// 运行期扫一个目录里的 `*.toml` 作为**外部层**：同 key 压过内置，缺的 key 照常
    /// 回落内置。目录不存在不是错误（多数安装不带外部译文）。
    ///
    /// 相对路径按**进程当前目录**解析。要跟着 exe 走，传
    /// `std::env::current_exe()` 推出来的绝对路径。
    pub fn load_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dirs.push(dir.into());
        self
    }

    /// 全局回退语言（默认 `en`）。单个文件可用 `[meta] fallback` 覆盖。
    pub fn fallback(mut self, id: impl Into<String>) -> Self {
        self.fallback = Some(id.into());
        self
    }

    /// 启动语言（默认 [`Initial::Fixed`]`("zh-CN")`）。
    pub fn initial(mut self, initial: Initial) -> Self {
        self.initial = Some(initial);
        self
    }

    pub fn build(self) -> Locales {
        let mut l = Locales {
            langs: BTreeMap::new(),
            fallback: self.fallback.unwrap_or_else(|| "en".into()),
            initial: self.initial.unwrap_or_default(),
            dirs: self.dirs,
        };
        // 框架自带文案铺最底层：应用的 `embed` 与外部文件都能用同名 `windui.*` key 压过它。
        for src in BUILTIN {
            match parse::parse(src) {
                Ok(f) => l.add_layer(f, None, true),
                Err(e) => {
                    log::error!("i18n: 框架自带译文解析失败：{e}");
                    debug_assert!(false, "框架自带译文解析失败：{e}");
                }
            }
        }
        for (f, id) in self.files {
            l.add_layer(f, id, true);
        }
        let dirs = l.dirs.clone();
        for dir in dirs {
            l.scan_dir(&dir);
        }
        l.warn_languages_without_framework_strings();
        l
    }
}

/// 某一种语言的查找视图（当前语言 + 回退语言已合并成一张表）。
///
/// 控件经 [`current()`] 拿到它。合并在换语言时做一次，查找就是一次哈希——不是每次查
/// 都走一遍"外部→内置→回退"的链。
#[derive(Debug, Clone)]
pub struct Catalog {
    lang: String,
    rtl: bool,
    entries: BTreeMap<String, Entry>,
}

impl Catalog {
    /// 空目录（所有 key 都 miss）。测试与"一种语言都没装"时用。
    pub fn empty(lang: impl Into<String>) -> Self {
        Self {
            lang: lang.into(),
            rtl: false,
            entries: BTreeMap::new(),
        }
    }

    /// 当前语言标签。
    pub fn lang(&self) -> &str {
        &self.lang
    }

    /// 当前语言是否从右向左书写（**渲染层尚不消费**，见 `docs/i18n-design.md` §9）。
    pub fn rtl(&self) -> bool {
        self.rtl
    }

    /// 原始条目。给 lint / 测试用。
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries.get(key)
    }

    /// 全部 key（升序）。给一致性测试用。
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(|s| s.as_str())
    }

    /// 无参取文。
    pub fn text(&self, key: &str) -> String {
        self.format(&Message::new(key.to_string()))
    }

    /// 取数组（key 不是数组则 `None`）。
    pub fn list(&self, key: &str) -> Option<Vec<String>> {
        match self.entries.get(key)? {
            Entry::List(v) => Some(v.iter().map(|p| p.format(&[], &[])).collect()),
            _ => None,
        }
    }

    /// 按当前语言把一条消息格式化出来。
    pub fn format(&self, m: &Message) -> String {
        let entry = match self.entries.get(m.key.as_ref()) {
            Some(e) => e,
            None => return miss(&m.key),
        };
        let pat = match entry {
            Entry::One(p) => Some(p),
            Entry::Variants(map) => self.pick_variant(map, m),
            Entry::List(v) => match m.arg(ARG_INDEX).and_then(|a| a.as_count()) {
                Some(i) if i >= 0 => v.get(i as usize),
                // 没给 index 的数组：当成用错了 key，按 miss 处理而不是拼出一串。
                _ => None,
            },
        };
        match pat {
            Some(p) => p.format(&m.named, &m.pos),
            None => miss(&m.key),
        }
    }

    /// 变体表选一条：`select` 参数优先，否则按 `count` 走复数规则；取不到落 `other`，
    /// 再取不到就拿表里第一条（并在 debug 下 warn）——译文写漏一个类别时，显示一条
    /// 语法不完美的句子远好过显示 `⟪key⟫`。
    fn pick_variant<'a>(
        &'a self,
        map: &'a BTreeMap<Box<str>, Pattern>,
        m: &Message,
    ) -> Option<&'a Pattern> {
        let cat: Cow<'static, str> = match m.arg(ARG_SELECT) {
            Some(v) => Cow::Owned(v.as_text()),
            None => match m.arg(ARG_COUNT).and_then(|a| a.as_count()) {
                Some(n) => Cow::Borrowed(plural::select(&self.lang, n).key()),
                None => Cow::Borrowed("other"),
            },
        };
        if let Some(p) = map.get(cat.as_ref()) {
            return Some(p);
        }
        if let Some(p) = map.get("other") {
            return Some(p);
        }
        if let Some((k, p)) = map.iter().next() {
            log::warn!(
                "i18n: `{}` 的变体表既没有 `{cat}` 也没有 `other`，退到 `{k}`",
                m.key
            );
            return Some(p);
        }
        None
    }
}

thread_local! {
    /// 已加载的全部语言。首次访问时懒构建成「只有框架自带译文」的那一份，
    /// 于是**应用什么都不配也能跑**（框架自带文案有着落，应用自己的 key 全 miss）。
    static LOCALES: RefCell<Option<Locales>> = const { RefCell::new(None) };
    /// 当前语言的查找视图。
    static CURRENT: RefCell<Option<Rc<Catalog>>> = const { RefCell::new(None) };
    /// 已经喊过的 miss key，去重用——不去重的话每帧每控件各来一条日志。
    static WARNED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

fn with_locales<R>(f: impl FnOnce(&mut Locales) -> R) -> R {
    LOCALES.with(|l| {
        let mut b = l.borrow_mut();
        f(b.get_or_insert_with(Locales::default))
    })
}

/// 装载译文目录并按 [`Initial`] 选定启动语言。
///
/// [`App::locales`](crate::app::App::locales) 会调它；不经 `App` 的场景（测试、
/// 自绘宿主）可以直接调。
pub fn install(locales: Locales) {
    let lang = locales.initial_lang();
    let cat = Rc::new(locales.catalog(&lang));
    LOCALES.with(|l| *l.borrow_mut() = Some(locales));
    CURRENT.with(|c| *c.borrow_mut() = Some(cat));
    WARNED.with(|w| w.borrow_mut().clear());
}

/// 当前语言的目录快照。控件在 `measure`/`paint` 里读它。
pub fn current() -> Rc<Catalog> {
    CURRENT.with(|c| {
        if let Some(cat) = c.borrow().as_ref() {
            return cat.clone();
        }
        let cat = with_locales(|l| {
            let lang = l.initial_lang();
            Rc::new(l.catalog(&lang))
        });
        *c.borrow_mut() = Some(cat.clone());
        cat
    })
}

/// 当前语言标签。
pub fn language() -> String {
    current().lang().to_string()
}

/// 已加载的语言列表（供设置页填下拉）。
pub fn available() -> Vec<LocaleInfo> {
    with_locales(|l| l.available())
}

/// key miss 的表现。分档是故意的：
/// - debug：`⟪key⟫` + 一次 warn。醒目的乱码比静默空串好——空串在界面上看着像
///   "这里本来就没字"，能瞒过整轮自测。
/// - release：key 本身。至少看得出是哪条漏了，且不拿尖括号吓用户。
fn miss(key: &str) -> String {
    let first = WARNED.with(|w| w.borrow_mut().insert(key.to_string()));
    if first {
        log::warn!("i18n: 缺少译文 `{key}`");
    }
    if cfg!(debug_assertions) {
        format!("⟪{key}⟫")
    } else {
        key.to_string()
    }
}

/// 运行期语言句柄：克隆进控件回调，`set` 即可热切换，下一帧整树跟随。
///
/// 从 [`App::locale_handle`](crate::app::App::locale_handle) 取，或直接
/// `LocaleHandle::new()`——与 [`ThemeHandle`](crate::app::ThemeHandle) 不同，它**不持有**
/// 任何东西（状态在线程局部里），所以不需要 `detached` 那样的测试专用构造口。
#[derive(Clone, Copy, Default)]
pub struct LocaleHandle {
    /// 负标记：使句柄成为 `!Send` + `!Sync`。语言状态在线程局部里，句柄跨线程一文不值，
    /// 把这点变成编译期约束（与 `Signal` 同一手法）。
    _not_send: PhantomData<*const ()>,
}

impl LocaleHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// 切到某种语言，返回是否命中了一个已加载的语言。
    ///
    /// 未命中时**保持当前语言不变**并 warn——切失败还把界面清成 key 列表，比不切更糟。
    pub fn set(&self, lang: &str) -> bool {
        let (hit, cat) = with_locales(|l| match l.resolve_lang(lang) {
            Some(id) => (true, Some(Rc::new(l.catalog(&id)))),
            None => (false, None),
        });
        if !hit {
            log::warn!("i18n: 没有加载语言 `{lang}`，维持 `{}`", language());
            return false;
        }
        CURRENT.with(|c| *c.borrow_mut() = cat);
        WARNED.with(|w| w.borrow_mut().clear());
        invalidate();
        true
    }

    /// 当前目录快照（等价 [`current()`]）。
    pub fn current(&self) -> Rc<Catalog> {
        current()
    }

    /// 当前语言标签。
    pub fn language(&self) -> String {
        language()
    }

    /// 已加载的语言（供设置页填下拉）。
    pub fn available(&self) -> Vec<LocaleInfo> {
        available()
    }

    /// 重扫 [`LocalesBuilder::load_dir`] 记下的目录并刷新当前语言。
    ///
    /// 不做文件监听：那要拉一个 watcher 依赖 + 一条后台线程，而"改完译文点一下刷新"
    /// 是开发期就够用的粒度。
    pub fn reload(&self) {
        // 先取语言再进闭包：`language()` → `current()` 在目录尚未建立时会去借 `LOCALES`，
        // 放在 `with_locales` 里面就是同一个 `RefCell` 的二次借用。
        let lang = language();
        let cat = with_locales(|l| {
            l.rescan();
            Rc::new(l.catalog(&lang))
        });
        CURRENT.with(|c| *c.borrow_mut() = Some(cat));
        WARNED.with(|w| w.borrow_mut().clear());
        invalidate();
    }
}

/// 换语言后的失效：与 [`ThemeHandle::set`](crate::app::ThemeHandle::set) 同两笔。
///
/// - `request_repaint`：本窗口请求一帧。**它走的是全窗路径**，而全窗路径必定
///   `layout_root` 重排（见 `UiHost::prepare_full_frame`）——换语言会改文本宽高，
///   属非局部变更，必须重排；沿用控件自报的局部脏区会让"标签变长顶动按钮"那一帧
///   只重画标签自己。
/// - `mark_cross_window_dirty`：目录为所有窗口共享，而 `request_repaint` 只唤起当前
///   窗口。少这一笔，换语言就只在应用碰巧同时写了信号时才联动到别的窗口（换肤踩过）。
///
/// 文字测量缓存**无需**在此清：它按 (文本, 样式) 哈希为键，旧语言的条目自然不再命中，
/// 且引擎侧有 4096 条上限会自行清空（见 `text/dwrite.rs` 的 `MEASURE_CACHE_CAP`）。
fn invalidate() {
    crate::anim::request_repaint();
    crate::signal::mark_cross_window_dirty();
}

/// `t!("key", name = value, …)` → [`Message`]，**进控件**，换语言自动跟随。
///
/// 参数三种写法可混用：
/// - `name = expr` 命名参数，对应译文里的 `{name}`；
/// - `expr` 位置参数，按出现顺序对应 `{0}` `{1}`；
/// - 保留名 `count`（选复数类别）、`select`（选变体）、`index`（取数组下标）。
///
/// ```
/// # use windui::prelude::*;
/// let _ = Element::label(t!("app.hello"));
/// let _ = Element::label(t!("app.moved", name = "a.txt", dest = "下载"));
/// let _ = Element::label(t!("app.files", count = 3));
/// ```
#[macro_export]
macro_rules! t {
    ($key:expr $(,)?) => {
        $crate::i18n::Message::new($key)
    };
    ($key:expr, $($args:tt)+) => {{
        let __msg = $crate::i18n::Message::new($key);
        $crate::__i18n_args!(__msg, $($args)+)
    }};
}

/// `tr!(...)` → `String`，**当场定格**，换语言不跟随。参数写法同 [`t!`]。
///
/// 用在需要 `String` 的地方：`MenuItem::run` 的标签、`ctx.toast`、日志、窗口标题。
/// 菜单每次弹出重建，所以定格无妨——这正是不把 `MenuItem.label` 改成 `TextContent`
/// 的原因（那是破坏性变更，收益为零）。
#[macro_export]
macro_rules! tr {
    ($($args:tt)*) => {
        $crate::i18n::Message::resolve(&$crate::t!($($args)*))
    };
}

/// [`t!`] 的参数消费器。不是公开 API。
#[doc(hidden)]
#[macro_export]
macro_rules! __i18n_args {
    ($m:expr, $n:ident = $v:expr $(,)?) => {
        $m.set(stringify!($n), $v)
    };
    ($m:expr, $n:ident = $v:expr, $($rest:tt)+) => {
        $crate::__i18n_args!($m.set(stringify!($n), $v), $($rest)+)
    };
    ($m:expr, $v:expr $(,)?) => {
        $m.push($v)
    };
    ($m:expr, $v:expr, $($rest:tt)+) => {
        $crate::__i18n_args!($m.push($v), $($rest)+)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZH: &str = r#"
[meta]
locale = "zh-CN"
name = "简体中文"
[app]
hello = "你好"
moved = "已把 {name} 移到 {dest}"
[app.files]
other = "已选 {count} 个文件"
[app.week]
days = ["一", "二"]
"#;

    const EN: &str = r#"
[meta]
locale = "en"
name = "English"
[app]
hello = "Hello"
moved = "Moved {name} to {dest}"
only_en = "only in en"
[app.files]
one = "{count} file selected"
other = "{count} files selected"
"#;

    fn locales() -> Locales {
        Locales::builder()
            .embed(ZH)
            .embed(EN)
            .fallback("en")
            .initial(Initial::Fixed("zh-CN".into()))
            .build()
    }

    /// 造一个只属于本测试的临时译文目录（同进程多测试并行，目录必须互不相干）。
    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "windui_i18n_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建临时目录");
        d
    }

    #[test]
    fn external_files_override_embedded_per_key() {
        // 需求的另一半：文案多的应用把译文放外部，**只交差异**。
        let dir = temp_dir("override");
        std::fs::write(
            dir.join("zh-CN.toml"),
            "[meta]\nlocale = \"zh-CN\"\n[app]\nhello = \"外部的你好\"\n",
        )
        .expect("写外部译文");

        let l = Locales::builder()
            .embed(ZH)
            .embed(EN)
            .load_dir(&dir)
            .fallback("en")
            .build();
        let c = l.catalog("zh-CN");

        assert_eq!(c.text("app.hello"), "外部的你好", "外部层应压过内置");
        assert!(
            !c.text("app.moved").is_empty(),
            "外部文件没写的 key 必须照常回落内置，而不是变空"
        );
        assert_eq!(
            c.format(&Message::new("app.moved").set("name", "a").set("dest", "b")),
            "已把 a 移到 b"
        );
        assert_eq!(
            c.text("app.only_en"),
            "only in en",
            "回退语言的链路不受影响"
        );
    }

    #[test]
    fn an_external_file_can_add_a_whole_new_language() {
        // 不重新编译就多一种语言——这是"外部加载"相对"内置"的全部意义。
        let dir = temp_dir("newlang");
        std::fs::write(
            dir.join("fr.toml"),
            "[meta]\nlocale = \"fr\"\nname = \"Français\"\n[app]\nhello = \"Bonjour\"\n",
        )
        .expect("写外部译文");

        let l = Locales::builder()
            .embed(ZH)
            .embed(EN)
            .load_dir(&dir)
            .build();
        assert!(l
            .available()
            .iter()
            .any(|i| i.id == "fr" && i.name == "Français"));
        assert_eq!(l.catalog("fr").text("app.hello"), "Bonjour");
    }

    #[test]
    fn a_broken_external_file_only_costs_that_one_file() {
        // 用户手改坏一个文件，不能让整个应用没文案——含内置译文与别的语言。
        let dir = temp_dir("broken");
        std::fs::write(dir.join("fr.toml"), "这不是 TOML = = =").expect("写坏文件");
        std::fs::write(
            dir.join("de.toml"),
            "[meta]\nlocale = \"de\"\n[app]\nhello = \"Hallo\"\n",
        )
        .expect("写好文件");

        let l = Locales::builder().embed(ZH).load_dir(&dir).build();
        assert_eq!(
            l.catalog("zh-CN").text("app.hello"),
            "你好",
            "内置译文不受牵连"
        );
        assert_eq!(
            l.catalog("de").text("app.hello"),
            "Hallo",
            "同目录的好文件照常生效"
        );
        assert!(!l.available().iter().any(|i| i.id == "fr"), "坏文件被跳过");
    }

    #[test]
    fn reload_picks_up_edited_files() {
        let dir = temp_dir("reload");
        let path = dir.join("zh-CN.toml");
        std::fs::write(
            &path,
            "[meta]\nlocale = \"zh-CN\"\n[app]\nhello = \"第一版\"\n",
        )
        .expect("写外部译文");
        install(Locales::builder().embed(ZH).load_dir(&dir).build());
        assert_eq!(tr!("app.hello"), "第一版");

        std::fs::write(
            &path,
            "[meta]\nlocale = \"zh-CN\"\n[app]\nhello = \"第二版\"\n",
        )
        .expect("改外部译文");
        assert_eq!(tr!("app.hello"), "第一版", "不主动 reload 就不该变");

        LocaleHandle::new().reload();
        assert_eq!(tr!("app.hello"), "第二版");
    }

    #[test]
    fn a_missing_external_dir_is_not_an_error() {
        // 多数安装不带外部译文，那是常态不是故障。
        let l = Locales::builder()
            .embed(ZH)
            .load_dir("绝对不存在的目录_windui")
            .build();
        assert_eq!(l.catalog("zh-CN").text("app.hello"), "你好");
    }

    /// 系统语言探测的**形状**断言。内容不能断言（取决于跑测试的机器），但解析错的
    /// 表现恰好都在形状上：win32 那份要从双 NUL 结尾的多串里切，切错就是空串或带着
    /// 尾巴的乱码；macOS 那份若把 `currentLocale` 当成 `preferredLanguages`，返回的
    /// 会是区域标识而不是语言列表。
    #[test]
    fn system_locales_are_well_formed() {
        let langs = crate::platform::system_locales();
        for l in &langs {
            assert!(!l.is_empty(), "语言标签不该是空串：{langs:?}");
            assert!(
                l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
                "BCP-47 标签只该有字母数字与连字符，得到 {l:?}（多半是缓冲区切错了）"
            );
            assert!(l.len() <= 35, "BCP-47 标签不会这么长：{l:?}");
        }
    }

    #[test]
    fn builtin_translations_stay_in_sync() {
        // 框架自带的这两份文件必须逐条对齐。信源是文件本身，不是解析器——
        // 这条测试红了就去改 `i18n/*.toml`，不是改代码。
        let problems = lint::check(BUILTIN);
        assert!(problems.is_empty(), "内置译文不一致：{problems:#?}");
    }

    /// 反方向的那一半：库源码里**写出来的**每个 `windui.*` key 都得有译文。
    ///
    /// 与下面那张手写清单是一对，缺一不可：
    /// - 手写清单查「译文里有没有」——删掉一处 `tr!` 调用它照样在查；
    /// - 这一条查「写出来的对不对」——`tr!("windui.menu.cutt")` 打错一个字母，
    ///   界面上只会安静地显示 `⟪windui.menu.cutt⟫`，没有任何编译期信号。
    ///
    /// 只看 `windui.*` 前缀：`src/` 里其余的 `t!("app.…")` 都是测试各自现装的目录，
    /// 不属于框架自带文案。
    #[test]
    fn framework_keys_written_in_the_source_all_have_translations() {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(Path::new("src"), &mut files);
        assert!(
            !files.is_empty(),
            "扫不到 src/*.rs：测试的工作目录不是 crate 根？"
        );

        let locales = Locales::default();
        let mut bad = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for path in files {
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            for u in lint::keys_in_source(&src) {
                if !u.key.starts_with("windui.") {
                    continue;
                }
                seen.insert(u.key.clone());
                if !locales.has_key(&u.key) {
                    bad.push(format!("{}:{} `{}`", path.display(), u.line, u.key));
                }
            }
        }
        assert!(bad.is_empty(), "源码里用了不存在的框架 key：{bad:#?}");
        // 下限断言：没有它，`keys_in_source` 一旦退化成恒返回空（词法缺陷、或某次重构
        // 把 `tr!("windui.…")` 藏到一个帮手函数后面），`bad` 恒为空、这条测试永远绿——
        // 而它存在的全部意义就是盯拼写错误。这正是「测试自证循环」那条教训的形态。
        assert!(
            seen.len() >= 8,
            "只扫到 {} 条 windui.* key：{seen:#?}——扫描器可能瞎了，这条测试就成了摆设",
            seen.len()
        );
    }

    #[test]
    fn builtin_covers_every_key_the_library_asks_for() {
        // 库自己 `tr!("windui.…")` 的每一条都必须存在。硬编码这张清单是有意的：
        // 从源码里 grep 出 key 再校验，等于让被测对象自己提供期望值（测试自证循环）；
        // 而漏迁一条的症状是界面上冒出 `⟪windui.menu.cut⟫`，值得一张手写清单盯着。
        let c = Locales::default().catalog("en");
        for key in [
            "windui.menu.cut",
            "windui.menu.copy",
            "windui.menu.paste",
            "windui.menu.select_all",
            "windui.window.restore",
            "windui.window.minimize",
            "windui.window.maximize",
            "windui.window.close",
        ] {
            assert!(
                c.entry(key).is_some(),
                "框架自带译文缺 `{key}`——界面上会显示成 ⟪{key}⟫"
            );
        }
    }

    #[test]
    fn builtin_is_present_without_any_app_translation() {
        // 应用什么都不配，框架自带文案也要有着落。
        let l = Locales::default();
        let c = l.catalog("zh-CN");
        assert_eq!(c.text("windui.menu.copy"), "复制");
        assert_eq!(l.catalog("en").text("windui.menu.copy"), "Copy");
    }

    #[test]
    fn app_translation_overrides_builtin_key() {
        let l = Locales::builder()
            .embed("[meta]\nlocale = \"zh-CN\"\n[windui.menu]\ncopy = \"拷贝\"\n")
            .build();
        assert_eq!(l.catalog("zh-CN").text("windui.menu.copy"), "拷贝");
    }

    #[test]
    fn missing_key_falls_back_to_the_fallback_language() {
        let c = locales().catalog("zh-CN");
        assert_eq!(c.text("app.hello"), "你好");
        assert_eq!(
            c.text("app.only_en"),
            "only in en",
            "本语言没有的 key 应落到回退语言，而不是 miss"
        );
    }

    #[test]
    fn missing_everywhere_is_marked_not_blank() {
        let c = locales().catalog("zh-CN");
        let got = c.text("app.nope");
        if cfg!(debug_assertions) {
            assert_eq!(got, "⟪app.nope⟫");
        } else {
            assert_eq!(got, "app.nope");
        }
        assert!(!got.is_empty(), "miss 绝不能是空串——那在界面上看不出异常");
    }

    #[test]
    fn unknown_language_falls_back_whole() {
        let c = locales().catalog("de");
        assert_eq!(c.lang(), "en", "没有 de 就该整体退到 fallback");
        assert_eq!(c.text("app.hello"), "Hello");
    }

    #[test]
    fn loose_language_matching() {
        let l = locales();
        assert_eq!(l.resolve_lang("zh-CN").as_deref(), Some("zh-CN"));
        assert_eq!(l.resolve_lang("ZH-cn").as_deref(), Some("zh-CN"));
        assert_eq!(
            l.resolve_lang("zh-Hans-CN").as_deref(),
            Some("zh-CN"),
            "系统报 zh-Hans-CN，译文文件叫 zh-CN，没有这级降级就永远匹配不上"
        );
        assert_eq!(l.resolve_lang("en-US").as_deref(), Some("en"));
        assert_eq!(l.resolve_lang("de").as_deref(), None);
    }

    #[test]
    fn plural_uses_the_catalog_language() {
        let l = locales();
        let en = l.catalog("en");
        assert_eq!(
            en.format(&Message::new("app.files").set("count", 1)),
            "1 file selected"
        );
        assert_eq!(
            en.format(&Message::new("app.files").set("count", 3)),
            "3 files selected"
        );
        let zh = l.catalog("zh-CN");
        assert_eq!(
            zh.format(&Message::new("app.files").set("count", 1)),
            "已选 1 个文件",
            "中文只有 other，1 也走 other"
        );
    }

    #[test]
    fn variant_table_without_the_needed_category_falls_to_other() {
        // zh 的表只有 other；即便按 en 的规则选出了 one，也该落回 other 而不是 miss。
        let l = locales();
        let zh = l.catalog("zh-CN");
        assert_eq!(
            zh.format(&Message::new("app.files").set("count", 1)),
            "已选 1 个文件"
        );
    }

    #[test]
    fn list_entry_is_indexed() {
        let c = locales().catalog("zh-CN");
        assert_eq!(
            c.list("app.week.days"),
            Some(vec!["一".into(), "二".into()])
        );
        assert_eq!(
            c.format(&Message::new("app.week.days").set("index", 1)),
            "二"
        );
    }

    #[test]
    fn named_args_survive_the_macro_path() {
        install(locales());
        assert_eq!(
            t!("app.moved", name = "a.txt", dest = "下载").resolve(),
            "已把 a.txt 移到 下载"
        );
        assert_eq!(tr!("app.hello"), "你好");
    }

    #[test]
    fn handle_switches_language_and_rejects_unknown() {
        install(locales());
        let h = LocaleHandle::new();
        assert_eq!(h.language(), "zh-CN");
        assert!(h.set("en"));
        assert_eq!(h.language(), "en");
        assert_eq!(tr!("app.hello"), "Hello");
        assert!(!h.set("de"), "未加载的语言应被拒绝");
        assert_eq!(
            h.language(),
            "en",
            "拒绝后维持原语言，不能把界面清成 key 列表"
        );
    }

    #[test]
    fn available_lists_display_names() {
        install(locales());
        let names: Vec<String> = available().into_iter().map(|i| i.name).collect();
        assert!(names.contains(&"简体中文".to_string()));
        assert!(names.contains(&"English".to_string()));
    }

    #[test]
    fn signal_arg_is_read_at_resolve_time() {
        install(locales());
        let n = crate::signal::signal(1i64);
        let m = t!("app.files", count = n);
        assert_eq!(m.resolve(), "已选 1 个文件");
        n.set(5);
        assert_eq!(m.resolve(), "已选 5 个文件", "信号参数应现取，不缓存");
    }
}
