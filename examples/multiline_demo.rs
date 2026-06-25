//! 多行长文本输入控件滚动压测示例。
//!
//! 运行：cargo run --release --example multiline_demo
//!
//! 测试场景：
//!   1. 短固定高度框 — 超出内容后滚动是否跟手
//!   2. 长行软换行 — 超宽单行是否自动折行（wrap 模式）
//!   3. 无换行（水平溢出）— 单行模拟；确认 scroll_x 正常
//!   4. 按钮动态追加内容 — 滚动锚点是否随光标保持可见
//!   5. 清空 — 回到顶部是否正确

use windui::prelude::*;

const FG: u32 = 0x1E2A3A;
const SUB: u32 = 0x6B7A8D;
const CARD: u32 = 0xFFFFFF;
const BG: u32 = 0xEFF2F7;
const ACCENT: u32 = 0x4C8BF5;

/// 初始长文本：包含短行、长行、空行，模拟真实用户输入。
const LONG_TEXT: &str = "\
第一行：普通短文本。
第二行：这是一段稍长的描述文字，用来验证软换行是否能在视觉上正确折回下一行而不截断。
第三行：A very long English sentence that keeps going on and on without any line break to test horizontal overflow and wrapping behavior in the text input control.

第五行（上方是空行）：验证空行不会导致布局崩溃。
第六行：Rust 是系统级编程语言，提供内存安全与高性能。
第七行：wind-ui-rust 使用 DirectWrite 进行文字排版。
第八行：GDI 渲染路径走 tiny-skia CPU 光栅化。
第九行：多行文本框的垂直滚动由 scroll_y 偏移驱动。
第十行：每次按键重新测量视觉行高度并约束偏移值。
第十一行：光标移动后框架自动将其滚入可见区域。
第十二行：鼠标拖拽选区时同样触发自动滚入（autoscroll）。
第十三行：Tab 键在多行模式下插入制表符（单行模式跳 focus）。
第十四行：按下 Home/End 可跳行首/行尾，Ctrl+Home 跳文首。
第十五行：测试到此为止，继续点【追加】按钮可以扩充内容。";

/// 追加用的补充段落。
const APPEND_TEXT: &str = "\n\n--- 追加段落 ---\n\
这一段是通过【追加段落】按钮动态插入的。\n\
目的：验证内容增加后滚动条范围是否实时更新。\n\
如果光标在末尾，追加后应自动滚到底部。\n\
如果光标在中间，追加后光标位置不变，滚动位置保持。";

fn section(title: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .bg(Color::hex(CARD))
        .corner(10.0)
        .padding(16)
        .spacing(10)
        .child(
            Element::label(title)
                .font_size(15.0)
                .fg(Color::hex(FG))
                .width_match(),
        )
        .child(Element::divider())
        .child(body)
}

fn hint(text: &str) -> Element {
    Element::label(text)
        .font_size(12.0)
        .fg(Color::hex(SUB))
        .width_match()
}

fn main() {
    // ── 场景 1：固定矮框，内容超出 → 测垂直滚动 ──────────────────────────
    let text1 = signal(String::from(LONG_TEXT));

    // ── 场景 2：wrap 模式，超宽长行自动折回 ─────────────────────────────
    let text2 = signal(String::from(
        "这是一行极长的文本，没有任何手动换行符。\
它会一直延伸，测试软换行（wrap）是否将超出宽度的部分自动折入下一视觉行，\
而不是截断或让用户水平滚动。English mixed: The quick brown fox jumps over \
the lazy dog. Rust is fast safe and productive. Wind UI renders text via \
DirectWrite on Windows with subpixel antialiasing and proper line metrics.",
    ));

    // ── 场景 3：高度更大的编辑框，模拟笔记类使用 ─────────────────────────
    let text3 = signal(String::from(
        "这里是笔记区域，高度更大。\n\
可以自由输入多行文本，测试：\n\
  - 光标上下移动是否正确切换视觉行\n\
  - Shift+方向键选区是否准确\n\
  - 双击选词是否正常\n\
  - 中文 IME 输入是否不乱码\n\
  - 长按 Backspace 连续删除是否稳定\n",
    ));

    // 追加按钮回调
    let t1_clone = text1;
    let t3_clone = text3;

    // ── 场景 4：空框，从零开始打字 ───────────────────────────────────────
    let text4 = signal(String::new());

    let body = Element::col()
        .width_match()
        .spacing(16)
        // ── 场景 1 ────────────────────────────────────────────────────────
        .child(section(
            "① 垂直滚动 — 固定矮框（height=120），内容 15 行",
            Element::col()
                .width_match()
                .spacing(8)
                .child(hint(
                    "框高固定 120px，初始内容超过 15 行。\
向下滚动鼠标滚轮 / 拖动滚动条 / 按 ↑↓ PgUp PgDn Ctrl+End 验证滚动。",
                ))
                .child(
                    Element::text_input(text1, "输入多行文字…")
                        .multiline()
                        .width_match()
                        .height(120),
                )
                .child(
                    Element::row()
                        .spacing(8)
                        .child(
                            Element::button("追加段落")
                                .accent(Color::hex(ACCENT))
                                .on_click(move |_| {
                                    t1_clone.update(|s| s.push_str(APPEND_TEXT));
                                }),
                        )
                        .child(Element::button("清空").neutral().on_click({
                            let t = text1;
                            move |_| t.update(|s| s.clear())
                        })),
                ),
        ))
        // ── 场景 2 ────────────────────────────────────────────────────────
        .child(section(
            "② 软换行（wrap）— 超宽长行自动折回，无水平滚动条",
            Element::col()
                .width_match()
                .spacing(8)
                .child(hint(
                    "单段超长文字，没有 \\n。\
wrap=true 时应在框宽处自动折行；\
框高 100px 时折出的多行同样可以垂直滚动。",
                ))
                .child(
                    Element::text_input(text2, "超宽长行…")
                        .multiline()
                        .width_match()
                        .height(100),
                ),
        ))
        // ── 场景 3 ────────────────────────────────────────────────────────
        .child(section(
            "③ 笔记区域 — 较大编辑框（height=200），测光标移动与选区",
            Element::col()
                .width_match()
                .spacing(8)
                .child(hint(
                    "高度 200px。\
重点测试：Shift+方向键选区、双击选词、\
Home/End 跳行首尾、Ctrl+A 全选，\
以及 IME 候选字期间滚动不跳位。",
                ))
                .child(
                    Element::text_input(text3, "在这里记笔记…")
                        .multiline()
                        .width_match()
                        .height(200),
                )
                .child(
                    Element::row()
                        .spacing(8)
                        .child(
                            Element::button("追加段落")
                                .accent(Color::hex(ACCENT))
                                .on_click(move |_| {
                                    t3_clone.update(|s| s.push_str(APPEND_TEXT));
                                }),
                        )
                        .child(Element::button("清空").neutral().on_click({
                            let t = text3;
                            move |_| t.update(|s| s.clear())
                        })),
                ),
        ))
        // ── 场景 4 ────────────────────────────────────────────────────────
        .child(section(
            "④ 从空白开始输入 — 验证首行渲染与回车换行",
            Element::col()
                .width_match()
                .spacing(8)
                .child(hint(
                    "初始为空。\
请手动输入多行文字（按 Enter 换行），\
验证首屏光标是否正确居顶、\
行高随输入增长是否触发滚动。",
                ))
                .child(
                    Element::text_input(text4, "在此输入，按 Enter 换行…")
                        .multiline()
                        .width_match()
                        .height(140),
                )
                .child(Element::button("清空").neutral().on_click({
                    let t = text4;
                    move |_| t.update(|s| s.clear())
                })),
        ));

    let ui = Element::col()
        .fill()
        .bg(Color::hex(BG))
        .padding(20)
        .spacing(0)
        .child(
            Element::label("多行文本输入 — 滚动压测")
                .font_size(22.0)
                .fg(Color::hex(FG))
                .height(36)
                .width_match(),
        )
        .child(
            Element::label("验证长内容垂直滚动、软换行、光标追踪、动态追加等场景")
                .font_size(13.0)
                .fg(Color::hex(SUB))
                .height(22)
                .width_match(),
        )
        .child(Element::divider())
        .child(
            Element::scroll().fill().child(
                Element::col()
                    .width_match()
                    .padding_xy(0, 14)
                    .spacing(0)
                    .child(body),
            ),
        );

    App::new("multiline_demo — 多行滚动压测", 560, 700)
        .bg(Color::hex(BG))
        .content(ui)
        .run();
}
