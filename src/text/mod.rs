//! 文字引擎抽象。Windows 下由 DirectWrite 实现（`dwrite`）；macOS 下由 Core Text 实现（`coretext`）。

#[cfg(windows)]
pub mod dwrite;
#[cfg(windows)]
pub use dwrite::{register_private_use_font, DWriteEngine};

#[cfg(target_os = "macos")]
pub mod coretext;
#[cfg(target_os = "macos")]
pub use coretext::CoreTextEngine;

/// 当前平台的具体文字引擎类型。`app` 层用此别名持有引擎，避免 `cfg` 散落到宿主逻辑里。
#[cfg(windows)]
pub type PlatformTextEngine = DWriteEngine;
#[cfg(target_os = "macos")]
pub type PlatformTextEngine = CoreTextEngine;

use tiny_skia::Pixmap;

use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

/// 默认字重（DirectWrite NORMAL = 400）。
pub const WEIGHT_NORMAL: u16 = 400;

/// 一次文字排版所需的全部字体属性。
///
/// ## 为什么是一个结构体而不是若干位置参数
///
/// 这些属性要穿过两层 trait（`Canvas` 与 `TextEngine`）、四个实现（skia / d2d /
/// DirectWrite / CoreText）和六十余处调用点。当初它们是散开的位置参数，于是每加一项
/// 都要改所有签名——字重就是因此**没有**进签名，改走线程局部注入的：核心层在
/// measure/paint 前 `set_weight`、之后复位。
///
/// 那条捷径有真实代价：字重成了隐式全局状态，一旦某条路径忘了复位，后续无关文字
/// 就会跟着变粗，且只在特定绘制顺序下显形。属性越多，这种耦合越危险。
///
/// 收进一个结构体后，新增属性只是加一个字段，签名不动、调用点不动；对控件代码
/// 反而更短——`&TextStyle::of(style)` 比原先的 `style.font_family.as_deref(),
/// style.font_size` 少写一截，且字重与行高自动随行，不会漏传。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextStyle<'a> {
    /// 字族名。`None` 用引擎默认。
    pub family: Option<&'a str>,
    /// 字号（逻辑单位 dp）。
    pub size: f32,
    /// 字重，`WEIGHT_NORMAL` 为常规。
    pub weight: u16,
    /// 斜体。与字重正交——字体里这是两个轴，故「粗斜体」是合法组合。
    pub italic: bool,
    /// 行高倍数（相对字号）。`None` 用字体自带的行距。
    ///
    /// 单位取**倍数**而非绝对像素，因为行距的本意是随字号缩放的排版节奏；写死像素
    /// 会在换字号时失调，也无法跨 DPI。
    pub line_height: Option<f32>,
}

impl<'a> TextStyle<'a> {
    /// 只指定字号，其余取默认。用于菜单、徽标等不随控件 `Style` 走的固定字号处。
    pub fn new(size: f32) -> Self {
        Self {
            family: None,
            size,
            weight: WEIGHT_NORMAL,
            italic: false,
            line_height: None,
        }
    }

    /// 从控件 `Style` 提取文字属性。控件绘制路径一律用它。
    pub fn of(style: &'a crate::style::Style) -> Self {
        Self {
            family: style.font_family.as_deref(),
            size: style.font_size,
            weight: style.font_weight,
            italic: style.font_italic,
            line_height: style.line_height,
        }
    }

    /// 换一个字号，其余不变。用于「比正文小两号」这类相对尺寸。
    pub fn with_size(self, size: f32) -> Self {
        Self { size, ..self }
    }

    /// 换一个字重，其余不变。
    pub fn with_weight(self, weight: u16) -> Self {
        Self { weight, ..self }
    }

    /// 换一个斜体设置，其余不变。
    pub fn with_italic(self, italic: bool) -> Self {
        Self { italic, ..self }
    }

    /// 行高解析为绝对像素；未指定行高时返回 `None`，由引擎沿用字体自带行距。
    pub fn line_height_px(&self) -> Option<f32> {
        self.line_height.map(|m| self.size * m)
    }
}

/// 一行文字的基线度量（逻辑 px）。`ascent`=基线以上高度、`descent`=基线以下高度
/// （含 leading），二者之和即该文字的自然行高。
///
/// 供富文本等**同行混字号**场景做基线对齐：行基线 = max(各段 ascent)，每段绘制
/// 矩形 top = 基线 − 自身 ascent、高 = 自身自然行高——引擎「矩形内垂直居中」的
/// 绘制约定在矩形高恰为自然行高时退化为顶对齐，字形即落在正确基线上。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineMetrics {
    pub ascent: f32,
    pub descent: f32,
}

impl LineMetrics {
    /// 自然行高（ascent + descent）。
    pub fn height(&self) -> f32 {
        self.ascent + self.descent
    }
}

/// 文字测量与绘制接口。测量供布局阶段，绘制供 paint 阶段合成进 pixmap。
///
/// 坐标/字号约定：对外接口均为**逻辑单位**（dp）。引擎内部按 DPI scale 物理化
/// （measure 物理排版后 /scale 回逻辑，draw 物理排版并按 rect×scale 合成），
/// 使测量与绘制走同一物理字号路径——字体 hinting 非线性，绝不可线性外推。
pub trait TextEngine {
    /// 设置 DPI 缩放因子。
    fn set_scale(&mut self, _scale: f32) {}
    /// 当前 DPI 缩放因子。测量结果随 scale 有物理取整差异，缓存了测量产物的
    /// 布局（如富文本）须把它计入缓存键，否则跨 DPI 显示器拖动后沿用旧几何。
    fn scale(&self) -> f32 {
        1.0
    }
    /// 文字尺寸。`max_width=None` 单行不换行；`Some(w)` 在宽度 w 内换行并返回多行尺寸。
    fn measure(&mut self, text: &str, ts: &TextStyle, max_width: Option<f32>) -> Size;
    /// `text` 按 `ts` 单行排版后的基线度量。传入实际文本（而非样本串）是有意的：
    /// 字体回退可能改变行度量，按实际内容询问才与 `draw` 同源。
    ///
    /// 默认实现按「基线 = 行高 × 0.8」近似——与 DirectWrite UNIFORM 行距的基线
    /// 约定一致，供 Null 引擎与尚未实现精确度量的引擎（Core Text 待接）使用。
    fn line_metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
        let h = self.measure(text, ts, None).h as f32;
        LineMetrics {
            ascent: h * 0.8,
            descent: h * 0.2,
        }
    }
    /// 在 `rect` 内按 `align` 水平对齐、按下述契约纵向定位，合成进 `pixmap`。
    /// `clip` 为可选裁剪矩形（滚动视口等），合成时仅写入该矩形内的像素。
    ///
    /// ## 纵向定位契约
    ///
    /// 文本块距 `rect` 顶部的偏移恒为 **`(rect.h - text_h).max(0) / 2`**：
    ///
    /// - `text_h <= rect.h`（装得下）→ 垂直居中；
    /// - `text_h > rect.h`（装不下）→ **顶对齐**，溢出部分交由调用方裁剪收口。
    ///
    /// `.max(0)` 不是可选的实现细节。它曾只写在 DirectWrite 一侧，Core Text 依
    /// 「垂直居中」的字面意思实现（差值为负即向上下对称溢出），于是同一张表格在
    /// Windows 上顶对齐截断、在 macOS 上却上下各露半行——两边都没违反当时的文档，
    /// 因为当时的文档只说了"垂直居中"，没规定装不下怎么办。歧义本身就是缺陷。
    ///
    /// 参考实现见 `block_offset_y`；各引擎可因整数/浮点取整差异内联等价表达式，
    /// 但语义必须一致。新引擎接入时按 `text_block_contract` 那组测试自测一遍。
    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        ts: &TextStyle,
        clip: Option<Rect>,
    );

    /// 本引擎的整段光栅能力（GPU 后端用）。默认 `None` = 不支持，调用方据此跳过
    /// GPU 文字绘制并提示一次；Core Text 引擎返回 `Some(self)`。
    ///
    /// 做成 accessor 而不是让 [`TextEngine`] 直接继承 [`GlyphSource`]：后者会强制
    /// 每个引擎都实现光栅（DirectWrite 侧根本用不到，只能写个 `unimplemented!()` 桩），
    /// 而「能不能」在这里是运行期要判的事——GPU 后端拿到的是 `&mut dyn TextEngine`，
    /// 静态类型上看不出对面是谁。
    fn glyph_source(&mut self) -> Option<&mut dyn GlyphSource> {
        None
    }
}

/// 一段文字光栅出来的 **A8 覆盖度位图**（物理像素，行优先，`data.len() == width * height`）。
///
/// 0 = 完全透明，255 = 完全覆盖。只有覆盖度、没有颜色：颜色由使用方（GPU 后端）在
/// 合成时调制，同一段文字换个颜色仍复用同一张位图——这是 run-cache 命中率的前提。
///
/// ## `pad` 存在的理由
///
/// 位图四周比**文本块**各宽出 `pad` 个物理像素。文本块是排版意义上的盒子
/// （单行 = ascent+descent+leading，折行 = framesetter 的建议尺寸），而字形是可以
/// 越出这个盒子的——带音调符的拉丁字母、斜体的右侧出挑、某些中文字体的竖钩。
/// 直接合成到目标缓冲的路径（`TextEngine::draw`）不受盒子约束，越出部分照画；
/// 光栅到独立位图时若按盒子裁，那部分墨就丢了，两条路径的字会长得不一样。
///
/// 定位时按 `block_size()` 算文本块的落点，再左上各退 `pad` 得到位图落点。
#[derive(Clone, Debug)]
pub struct AlphaMask {
    /// 覆盖度字节，行优先。
    pub data: Vec<u8>,
    /// 位图物理宽（含两侧 `pad`）。
    pub width: u32,
    /// 位图物理高（含上下 `pad`）。
    pub height: u32,
    /// 四周为字形出挑留的物理像素余量。
    pub pad: u32,
    /// 文本块的**未取整**物理尺寸。位图必须是整像素，排版尺寸却不是——`ceil` 到整数
    /// 再拿去算垂直居中，会给出恒为负的半像素偏置（`(rect_h - ceil(h))/2` 恒不大于
    /// `(rect_h - h)/2`），最终表现为 GPU 路径的整段文字比软后端**稳定高一个像素**。
    /// 实测就是这么发现的：墨量逐字节一致，墨迹范围却整体上移 1px。
    ///
    /// 故定位一律用这一对浮点值，位图尺寸只用来确定四边形多大。
    pub block: (f32, f32),
    /// 首行基线距**文本块顶**的物理距离（未取整）。
    ///
    /// 存在的理由是平台光栅器会把基线**向下取整吸附到整数行**（Core Text 实测如此：
    /// 同一段字在任意小数基线上光栅出的像素逐字节相同，只是整行平移）。于是「把位图贴到
    /// 取整后的块位置」并不等价于「让基线落在软后端那一行」——两次取整各自进位，差出来
    /// 的一个像素只随容器高的奇偶翻转，肉眼看就是 GPU 模式下文字整体高一行。
    ///
    /// 调用方按 `floor(块顶 + ascent) - floor(pad + ascent)` 定位，两边的基线行于是
    /// 恒等。详见 `render/gpu/canvas.rs` 的 `draw_text`。
    pub ascent: f32,
}

/// 一次整段光栅的全部输入。
///
/// 收进结构体的理由与 [`TextStyle`] 那条一样：这些字段同时是 [`GlyphSource::raster_run`]
/// 的入参**和** run-cache 的键，两处必须逐字段对齐——散成位置参数时，新增一项而忘了
/// 进键，症状是「改了某个属性画面不变」，且只在缓存命中时显形。
#[derive(Clone, Copy, Debug)]
pub struct RunRequest<'a> {
    pub text: &'a str,
    pub style: TextStyle<'a>,
    /// 水平对齐。**只影响折行时各行在文本块内的相对位置**；文本块本身在 rect 内
    /// 怎么放，由调用方按同一个 `align` 决定（见 [`block_offset_y`] 附近的契约）。
    pub align: Align,
    /// 排版宽度上限（**逻辑** px，通常即目标 rect 的宽）。装得下就单行，装不下按它折行。
    pub max_width: f32,
    /// DPI 缩放因子。显式传而不是读引擎里的 `scale()`：它同时是缓存键的一部分，
    /// 键与光栅必须同源——引擎内部 scale 与调用方 scale 不一致时，隐式取值会缓存出
    /// 一张尺寸对不上的位图，而键上看不出任何异常。
    pub scale: f32,
}

/// 把整段文字光栅成 alpha mask 的窄接口——GPU 后端与平台文字栈之间**唯一**的缝。
///
/// 分出这个 trait 而不是往 [`TextEngine`] 上加方法，是因为两者的实现者集合不同：
/// 排版/度量每个平台都得有，而「光栅成独立位图」只有走 GPU 合成的平台需要
/// （Windows 走 D2D 后端，`dwrite.rs` 不实现它）。窄到只有一个方法，将来 Linux 接
/// FreeType 时要照着做的事一目了然。
///
/// 字形像素**永远**由平台 API 生成，GPU 只负责上传与混合——「文字与系统一致」这个
/// 卖点就落在这一条上。
pub trait GlyphSource {
    /// 按 `req` 排版并光栅。文本为空、尺寸退化或超出实现的体量上限时返回 `None`
    /// （调用方据此跳过本次绘制，不是错误）。
    fn raster_run(&mut self, req: &RunRequest) -> Option<AlphaMask>;

    /// 按 `req` 排版成**字形序列**，不光栅。返回 `None` 表示这一段交不出字形序列
    /// （实现不支持、或这一段的形状超出它能处理的范围），调用方退回 [`Self::raster_run`]。
    ///
    /// 分出这一步是 glyph atlas 的前提：整段光栅的粒度下，一个按钮标签、一行表格文本
    /// 各占一张纹理，动态文本（输入框逐字变化）每帧都不命中；降到单字形之后，同一个
    /// 字形在整个界面里只光栅一次、只占 atlas 里的一小格。
    ///
    /// **排版仍然整段做**（kerning、连字、字体回退都依赖上下文），这里交出来的是排版
    /// 的结果而不是绕开排版。
    fn shape_run(&mut self, req: &RunRequest) -> Option<ShapedRun> {
        let _ = req;
        None
    }

    /// 光栅单个字形。[`Self::shape_run`] 交出过的键都必须能光栅出来。
    fn raster_glyph(&mut self, key: &GlyphKey) -> Option<GlyphBitmap> {
        let _ = key;
        None
    }
}

/// 水平亚像素相位的档数。
///
/// 平台光栅器的字形水平定位是**亚像素**的（Core Text 实测不吸附整数列，只有基线吸附
/// 整数行）。整段光栅时每个字形按 `frac(origin.x)` 落笔；拆成单字形后若一律贴整数列，
/// 字间距会肉眼可见地变化——「文字与系统一致」这个卖点最容易在这里丢掉。故每个字形按
/// 相位各存一份位图。
///
/// **4 档不是"够用就好"，是 CG 自己的粒度**：把它调到 8 档，重组判据里
/// `Wave AVA To. jgpq` 的墨量差从 0.01% 涨到 0.23%——多出来的那半档偏移会被 CG 舍回
/// 1/4，于是这边算的相位与它实际用的对不上。细化相位在这里是**反向**优化。
pub const SUBPIXEL_PHASES: u8 = 4;

/// atlas 里一个字形的身份。
///
/// 不含颜色（颜色在片元里调制），不含位置（位置在 [`PlacedGlyph`] 上）——同一个字形
/// 在一段文字里出现多次、在不同段文字里出现，共用同一张位图，这正是 atlas 的全部意义。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct GlyphKey {
    /// 字体身份：平台给的**稳定名字**（Core Text 取 PostScript 名）。
    ///
    /// 不用字体对象的指针值：指针会随对象释放被复用，键于是误命中到另一个字体，而
    /// 现场看不出任何异常（图片纹理缓存用指针当键踩过同一个坑）。一段文字里出现多个
    /// 字体是常态——字体回退对每个缺字的字符都会换一个物理字体。
    pub font: std::sync::Arc<str>,
    /// **物理**字号的位模式（`f32` 没有 `Eq`/`Hash`）。
    pub size: u32,
    /// 字体内的字形索引。
    pub glyph: u16,
    /// 水平亚像素相位，`0..`[`SUBPIXEL_PHASES`]。
    pub phase: u8,
}

/// 一个已定位的字形。位置全是**整数**物理像素：亚像素的那一份已经烘进
/// [`GlyphKey::phase`] 对应的那张位图里了。
#[derive(Clone, Debug)]
pub struct PlacedGlyph {
    pub key: GlyphKey,
    /// 字形原点（基线上的落笔点）相对文本块左边的物理列。
    pub x: i32,
    /// 该字形的基线相对**首行基线**的物理行偏移（单行且无上下标时恒 0）。
    ///
    /// 不直接给「相对块顶的行」：块顶在排版时还是浮点，而基线要吸附整数行——取整
    /// 必须与整段光栅那条路径在**同一个地方**做（`floor(块顶 + ascent)`），否则两次
    /// 取整各自进位，差出来的一个像素只随容器高的奇偶翻转。那正是 `AlphaMask::ascent`
    /// 文档记下的那个坑。
    pub dy: i32,
}

/// 一段文字排版后的字形序列。块度量的语义与 [`AlphaMask`] 逐字相同，好让两条路径
/// （atlas 与整段光栅）的放置逻辑共用同一份代码。
#[derive(Clone, Debug)]
pub struct ShapedRun {
    pub glyphs: Vec<PlacedGlyph>,
    /// 同 [`AlphaMask::block`]。
    pub block: (f32, f32),
    /// 同 [`AlphaMask::ascent`]。
    pub ascent: f32,
}

/// 单个字形的 A8 覆盖度位图 + 它相对**字形原点**的偏移。
///
/// 偏移必须由光栅方给出：字形的墨迹相对原点可上可下、可左可右（`j` 的下伸、`f` 的
/// 左出挑），调用方无从推算。
#[derive(Clone, Debug)]
pub struct GlyphBitmap {
    /// 覆盖度字节，行优先，`data.len() == width * height`。
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 位图左边相对字形原点的物理列偏移（可负）。
    pub left: i32,
    /// 位图顶边相对基线的物理行偏移，**向下为正**（故通常为负——墨在基线之上）。
    pub top: i32,
}

/// 占位引擎：不渲染，按等宽近似估算尺寸。供无 DirectWrite 的单元测试使用。
pub struct NullTextEngine;

impl TextEngine for NullTextEngine {
    fn measure(&mut self, text: &str, ts: &TextStyle, _max_width: Option<f32>) -> Size {
        let w = (text.chars().count() as f32 * ts.size * 0.6).ceil() as i32;
        // 行高照实反映到高度上：这是单测唯一能观察到行高生效的地方（真实引擎在
        // 无 DirectWrite 的测试环境下跑不起来），故这里不能图省事忽略它。
        let h = ts.line_height_px().unwrap_or(ts.size);
        Size::new(w, h.ceil() as i32)
    }
    fn draw(
        &mut self,
        _pixmap: &mut Pixmap,
        _text: &str,
        _rect: Rect,
        _color: Color,
        _align: Align,
        _ts: &TextStyle,
        _clip: Option<Rect>,
    ) {
    }
}

/// 测试用文字引擎：按 `\n` 显式换行与 `max_width` 折行，高度 = 行数 × 行高。
///
/// 与 [`NullTextEngine`] 互补——后者恒按单行返回，凡是依赖"文本变高"的布局
/// （滚动区是否溢出、限高是否生效）用它测都会失真：内容永远只有一行高，
/// 滚动条永远不出现，断言于是测了个寂寞。真实引擎（DirectWrite/CoreText）在
/// 无窗口的测试环境下跑不起来，故以此桩近似其换行语义。
///
/// 宽度估算沿用 `NullTextEngine` 的 `字数 × 字号 × 0.6`，不追求与真实字体度量一致，
/// 只保证"文本越长、行数越多、高度越大"这一单调性成立。
pub struct LineAwareTextEngine;

impl TextEngine for LineAwareTextEngine {
    fn measure(&mut self, text: &str, ts: &TextStyle, max_width: Option<f32>) -> Size {
        let char_w = ts.size * 0.6;
        let line_h = ts.line_height_px().unwrap_or(ts.size);
        let mut lines = 0usize;
        let mut widest = 0.0f32;
        for seg in text.split('\n') {
            let w = seg.chars().count() as f32 * char_w;
            match max_width {
                // 超出可用宽 → 按整宽折行（向上取整）。
                Some(mw) if mw > 0.0 && w > mw => {
                    lines += (w / mw).ceil() as usize;
                    widest = widest.max(mw);
                }
                _ => {
                    lines += 1;
                    widest = widest.max(w);
                }
            }
        }
        Size::new(
            widest.ceil() as i32,
            (lines.max(1) as f32 * line_h).ceil() as i32,
        )
    }
    fn draw(
        &mut self,
        _pixmap: &mut Pixmap,
        _text: &str,
        _rect: Rect,
        _color: Color,
        _align: Align,
        _ts: &TextStyle,
        _clip: Option<Rect>,
    ) {
    }
}

/// 文本块距 `rect` 顶部的偏移——[`TextEngine::draw`] 纵向定位契约的参考实现。
///
/// 引擎内部可因整数/浮点取整差异内联等价表达式（DirectWrite 走 i32 截断除法，
/// Core Text 走 f64），但**语义必须与本函数一致**：装得下垂直居中、装不下顶对齐。
// 生产调用点是 d2d 的 `draw_text`；关掉 `d2d` feature 或非 Windows 目标下只剩
// 契约测试用它，此时不算未使用。
#[cfg_attr(not(all(windows, feature = "d2d")), allow(dead_code))]
pub(crate) fn block_offset_y(rect_h: f32, text_h: f32) -> f32 {
    (rect_h - text_h).max(0.0) / 2.0
}

/// 纵向定位契约的跨引擎测试：Windows 跑 DirectWrite、macOS 跑 Core Text，
/// **同一份断言**。两边的实现细节可以不同，可观察的纵向行为不许不同。
#[cfg(test)]
mod text_block_contract {
    use super::*;

    /// 白底上 `[y0, y1)` 行区间内的非白像素数（墨量）。
    ///
    /// 判据用墨量而非"某个像素等于某色"：抗锯齿让边缘像素取值随引擎浮动，
    /// 逐像素比对会把两个都正确的实现判成不一致，而"这片区域有没有字"是稳定的。
    fn ink(pm: &Pixmap, y0: i32, y1: i32) -> usize {
        let w = pm.width() as i32;
        let data = pm.data();
        let mut n = 0;
        for y in y0.max(0)..y1.min(pm.height() as i32) {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                // 白底 (255,255,255)：任一通道偏离即算墨。
                if data[i] != 255 || data[i + 1] != 255 || data[i + 2] != 255 {
                    n += 1;
                }
            }
        }
        n
    }

    fn white(w: u32, h: u32) -> Pixmap {
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        pm
    }

    /// 参考实现自身的两个分支。
    #[test]
    fn block_offset_y_clamps_at_zero() {
        assert_eq!(block_offset_y(100.0, 40.0), 30.0, "装得下应居中");
        assert_eq!(block_offset_y(16.0, 42.0), 0.0, "装不下应顶对齐而非负偏移");
    }

    /// 文本高于容器时**顶对齐**：容器顶边以上不得有墨。
    ///
    /// 这正是 macOS 表格多行文本与 Windows 不一致的那个缺陷——Core Text 侧缺
    /// `.max(0)`，负偏移让文本以容器中心为中心上下对称溢出，容器上方于是出现半行字。
    #[test]
    fn overflowing_text_is_top_aligned() {
        let mut eng = PlatformTextEngine::default();
        eng.set_scale(1.0);
        let mut pm = white(120, 120);
        // 三行硬换行（不依赖各引擎的折行算法），容器只有 16px 高，必然装不下。
        let rect = Rect::new(10, 40, 90, 16);
        eng.draw(
            &mut pm,
            "AAA\nBBB\nCCC",
            rect,
            Color::rgb(0, 0, 0),
            Align::Start,
            &TextStyle::new(12.0),
            None,
        );
        assert!(
            ink(&pm, 40, 120) > 0,
            "正控：容器顶边以下应当有字，否则本测试没测到绘制"
        );
        assert_eq!(
            ink(&pm, 0, 40),
            0,
            "容器顶边以上不得有墨：装不下时须顶对齐，不得居中溢出"
        );
    }

    /// 装得下时仍**垂直居中**：确认上面的钳制没把正常情形一起改成顶对齐。
    #[test]
    fn fitting_text_stays_centered() {
        let mut eng = PlatformTextEngine::default();
        eng.set_scale(1.0);
        let mut pm = white(120, 120);
        // 单行文本放进 60px 高的容器：上下应各留出可观的空白。
        let rect = Rect::new(10, 20, 90, 60);
        eng.draw(
            &mut pm,
            "Ag",
            rect,
            Color::rgb(0, 0, 0),
            Align::Start,
            &TextStyle::new(12.0),
            None,
        );
        assert!(ink(&pm, 20, 80) > 0, "正控：容器内应当有字");
        // 12px 字放进 60px 容器，居中后上下各约 24px 空白；留足余量取 12px。
        assert_eq!(ink(&pm, 20, 32), 0, "居中时容器上部应留白");
        assert_eq!(ink(&pm, 68, 80), 0, "居中时容器下部应留白");
    }
}
