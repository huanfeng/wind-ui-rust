# 图片支持设计方案（windui）

- 日期：2026-06-17
- 状态：已确认，待实现
- 范围：为 windui 增加图片渲染能力，下沉为可被任意控件复用的内容原语，并预留多格式解码框架。

## 1. 目标与非目标

### 目标
- 新增图片渲染能力，支持三种来源：文件路径、嵌入字节（`include_bytes!`）、原始 RGBA 缓冲。
- 将图片能力**下沉为可复用内容原语 `ImageContent`**，使任意自绘控件可低成本嵌入图片（图标等）。
- 提供独立 `ImageView` 控件作为原语的薄包装。
- 支持多种 Fit 缩放模式（Contain/Cover/Fill/None），默认 Contain。
- 支持圆角裁剪（头像/卡片场景）。
- **预留多格式解码框架**：定义可插拔 `ImageDecoder` trait + 注册表，核心只内置 PNG（零新依赖），将来 JPEG/WebP 等由使用方注册，核心代码与 API 零破坏。

### 非目标（v1 边界，有意收窄）
- 不内置 PNG 之外的解码器（但框架就位，可由使用方扩展）。
- 不含 GIF 动图、SVG、网络加载。
- 控件接入仅做 **Button 图标** 一个示范；ListView 等其它控件本版不接（后续照抄 pattern 即可）。

## 2. 设计原则对齐

- **Widget 纯内容、不访问树**：图片的"解码 + Fit + 测量 + 绘制 + 占位 + 圆角"全部逻辑沉到 Widget 之下的 `ImageContent` 内容原语；`ImageView` 与 `Button` 图标共用同一份代码，零重复。
- **轻量零依赖红线**：核心仅用 tiny-skia 原生 PNG 解码，不引入新 crate。
- **逻辑/物理坐标双轨**：图片绘制走与图形同一条 `×scale` 物理化路径，保证高 DPI 不糊。
- **构建期解码、paint 期零解码**：资源在 `Element::image*()` 构建期一次性解码缓存，每帧只做 blit。
- **误用检测文化**：`.fit()`/`.corner()` 等图片专属修饰符复用现有 `config_text_input` 的 downcast 误用检测 pattern（链到非图片控件时 debug panic，release 静默忽略）。

## 3. 架构分层

三层，逐层下沉：

```
第 1 层  render::image::Image          解码后的不可变资源（Rc<Pixmap> 缓存）
          + 解码框架（ImageDecoder / 注册表 / PngDecoder）
第 2 层  render::image::ImageContent   可复用内容原语（被嵌入其它控件的那块）
          + Canvas::draw_image 图元（SkiaCanvas 实现）
第 3 层  ui::ImageView                 独立控件（ImageContent 薄包装）
          ui::Button 图标               接入示范（内嵌 Option<ImageContent>）
          Element::image* / .fit / .corner / .icon*   Builder API
```

## 4. 第 1 层：`Image` 资源 + 解码框架

文件：`src/render/image.rs`

### 4.1 资源类型

```rust
pub struct Image {
    pixmap: Rc<Pixmap>,  // 解码后缓存，Rc 共享，paint 期零解码
    w: u32,
    h: u32,
}

impl Image {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ImageError>;   // 读字节 → from_bytes（格式自适配）
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ImageError>;            // 嗅探格式 + 分发解码器
    pub fn from_png_bytes(bytes: &[u8]) -> Result<Self, ImageError>;        // 便捷直通 PNG（绕过注册表）
    pub fn from_rgba(w: u32, h: u32, rgba: &[u8]) -> Result<Self, ImageError>; // 原始 RGBA8（校验 len==w*h*4）
    pub fn size(&self) -> Size;   // 固有逻辑尺寸（1 图片像素 = 1 逻辑 dp）
}
```

### 4.2 解码框架（预留扩展点）

```rust
/// 解码产物：RGBA8（非预乘），由 Image 内部转 Pixmap（预乘）。
pub struct DecodedImage { pub width: u32, pub height: u32, pub rgba: Vec<u8> }

/// 可插拔解码器。核心内置 PNG；JPEG/WebP 等由使用方注册。
pub trait ImageDecoder {
    fn probe(&self, bytes: &[u8]) -> bool;                       // 按魔数判格式
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError>;
    fn name(&self) -> &'static str;                              // 诊断用
}

struct PngDecoder;  // v1 唯一内置，用 tiny-skia Pixmap::decode_png

/// 注册一个额外解码器（如包 image crate 的 JpegDecoder）。
pub fn register_decoder(d: Box<dyn ImageDecoder>);
```

- **注册表**：照搬 `theme` 的 thread-local 模式（UI 单线程，避免加锁），PNG 预置，保证 `from_png_*` 永远可用。
- **`from_bytes` 分发**：遍历注册表，首个 `probe` 命中的解码器负责 `decode`；无命中返回 `ImageError::UnsupportedFormat`。
- **扩展示例**（不进核心代码）：

```rust
windui::render::image::register_decoder(Box::new(MyJpegDecoder)); // 内部包 image crate
```

### 4.3 Fit 与错误类型

```rust
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    #[default] Contain,  // 等比缩放完整显示（可能留白）
    Cover,               // 等比缩放铺满、裁掉溢出
    Fill,                // 非等比拉伸填满
    None,                // 原始像素 1:1（受 DPI 缩放）
}

#[derive(Debug)]
pub enum ImageError {
    Io(std::io::Error),       // 文件读取失败
    UnsupportedFormat,        // 无解码器能 probe 命中
    Decode(String),           // 解码器内部错误
    InvalidRgba,              // from_rgba 长度与 w*h*4 不符
}
```

## 5. 第 2 层：`ImageContent` 内容原语 + `draw_image` 图元

### 5.1 `ImageContent`（核心——被嵌入其它控件的那块）

不碰树的纯内容结构体：

```rust
pub struct ImageContent {
    image: Option<Image>,   // None = 加载失败 / 空
    fit: Fit,
    radius: f32,            // 圆角裁剪半径（0 = 直角）
}

impl ImageContent {
    pub fn new(image: Option<Image>) -> Self;       // 持有解码结果（失败传 None）
    pub fn fit(self, fit: Fit) -> Self;
    pub fn corner(self, radius: f32) -> Self;
    pub fn intrinsic_size(&self) -> Size;            // 供控件 measure；None 时返回占位默认尺寸
    pub fn paint_into(&self, dst: Rect, canvas: &mut dyn Canvas, style: &Style); // 供控件 paint
}
```

- `paint_into`：有图 → `canvas.draw_image(img, dst, fit, radius)`；无图 → 画**淡灰底 + 边框占位框**（错误可见，而非静默消失）。

### 5.2 Canvas 新图元

`src/render/mod.rs` 的 `Canvas` trait 新增：

```rust
fn draw_image(&mut self, img: &Image, dst: Rect, fit: Fit, radius: f32);
```

`SkiaCanvas` 实现（`src/render/skia.rs`）：
- 用 `Pixmap::draw_pixmap`，`PixmapPaint { quality: FilterQuality::Bilinear, .. }` 平滑缩放。
- 按 `fit` 计算 src→dst 的 `Transform`（缩放 + 居中平移），叠加全局 `×scale`。
- **始终裁剪到 dst 框**（Cover 溢出、None 超框都安全收口）：复用裁剪栈，与栈顶 mask 求交。
- 圆角：`radius > 0` 时用 `rounded_rect_path` 构造圆角 mask（物理坐标，与现有 clip 同源），与当前裁剪 mask 求交后作为 `draw_pixmap` 的 mask。

> 说明：`Canvas::draw_image` 签名引用 `render::Image` 本地类型，不向控件层泄漏 tiny-skia；而 render 层（skia.rs）本就依赖 tiny-skia，故 `Image` 内包 `Pixmap` 是务实且一致的。

## 6. 第 3 层：控件与 Builder

### 6.1 `ImageView` 独立控件

文件：`src/ui/image.rs`

```rust
pub struct ImageView { content: ImageContent }

impl Widget for ImageView {
    fn measure(&self, _avail, _style, _text) -> Size { self.content.intrinsic_size() }
    fn paint(&self, _bounds, content, _focused, canvas, style) {
        self.content.paint_into(content, canvas, style);
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> { Some(self) } // 供 .fit()/.corner() 配置
}
```

### 6.2 Button 图标接入示范

`Button` 增加可选图标字段，证明"其它控件低成本接图片"的 pattern：

```rust
pub struct Button {
    label: String,
    icon: Option<ImageContent>,   // 新增
    state: BtnState,
    on_click: Option<ClickFn>,
}
```

- `measure`：有图标时在文字宽度基础上加 `图标宽 + 间距`，高度取 `max(文字高, 图标高)`。
- `paint`：图标画在文字左侧（icon rect + label rect 水平排布，整体居中）。

### 6.3 Builder API

`src/ui/mod.rs`：

```rust
Element::image(path)                         // 文件路径
Element::image_bytes(&[u8])                  // 嵌入字节（嗅探格式）
Element::image_rgba(w, h, &[u8])             // 原始 RGBA
    .fit(Fit::Cover)                         // 缩放模式（默认 Contain）
    .corner(8.0)                             // 圆角裁剪

Element::button("Save").icon_bytes(&[u8])    // Button 图标接入示范
```

- 构造函数内部调 `Image::from_*`，失败时 `ImageContent::new(None)`（占位框可见），不 panic。
- `.fit()` / `.corner()`：复用 `config_text_input` 的 downcast 误用检测 pattern（链到非图片控件 debug panic）。

> 注：`.corner()` 与现有 `Element::corner()`（设置 `Style.corner_radius`）语义需区分——图片圆角作用于像素裁剪。实现时让图片控件的 `.corner()` 同时写入 `ImageContent.radius`；非图片控件维持原 `Style.corner_radius` 行为。

## 7. 测量约定

- `intrinsic_size()` 返回图片像素尺寸作逻辑 dp（1 px = 1 dp，再由 DPI 缩放）。
- `Wrap` 尺寸时框 = 固有尺寸，此时 Contain == None。
- 加载失败时返回一个占位默认尺寸（如 48×48 dp），保证布局不塌陷。

## 8. 测试策略

### 单元测试
- 解码内嵌小 PNG（`include_bytes!` 一张 2×2 测试图）→ 验证 `size()`。
- `from_rgba` 长度校验：正确尺寸成功、错误长度返回 `InvalidRgba`。
- `from_bytes` 格式嗅探：PNG 命中 PngDecoder；非图片字节返回 `UnsupportedFormat`。
- 四种 Fit 的 src→dst 变换数学（纯函数，独立可测）。
- 圆角 mask 生成（radius>0 时角落像素被裁掉）。
- 占位 fallback：`ImageContent::new(None)` 的 `paint_into` 画出边框、`intrinsic_size` 返回默认尺寸。
- Button 带图标的 `measure`（宽度含图标 + 间距）。

### 截图验证
- `examples/image.rs`：展示三种来源 × 四种 Fit × 圆角 × 占位，配合 `--screenshot <path>` 出图肉眼核对。

### 接入验证（dev-conventions 约定）
- 接入 **showcase 控件 tab**（`examples/fullshowcase.rs`），新增图片演示分区。

## 9. 文件改动清单

| 文件 | 改动 |
|---|---|
| `src/render/image.rs` | 新增：`Image` / `DecodedImage` / `ImageDecoder` / `PngDecoder` / 注册表 / `Fit` / `ImageError` |
| `src/render/mod.rs` | `Canvas` trait 新增 `draw_image`；导出 image 模块类型 |
| `src/render/skia.rs` | `SkiaCanvas` 实现 `draw_image`（draw_pixmap + Fit 变换 + 圆角 mask + 裁剪） |
| `src/ui/image.rs` | 新增：`ImageContent` 内容原语 + `ImageView` 控件 |
| `src/ui/mod.rs` | Builder：`image`/`image_bytes`/`image_rgba`/`.fit`/`.corner`/`.icon_bytes`；Button 加 icon 字段 |
| `src/lib.rs` (prelude) | 导出 `Fit`、`ImageView`/`Image` 等公共类型 |
| `examples/image.rs` | 新增示例 |
| `examples/fullshowcase.rs` | showcase 图片分区 |
| `docs/API_GUIDE.md` / `docs/ROADMAP.md` | 文档同步 |

## 10. 未来扩展（非本版）

- 内置或社区 JPEG/WebP 解码器（通过 `register_decoder`，零核心改动）。
- GIF 动图、SVG 矢量。
- 异步/网络图片加载与占位过渡。
- 更多控件接入图标（ListView 行、CheckBox、Tab 等，照抄 Button pattern）。
