//! 斜体的视觉验证。单元测试只能证明 `italic` 传到了 TextStyle，证明不了
//! DirectWrite 真的换了字形——那必须看图。
use windui::prelude::*;
use windui::ui::rich::{Para, RichDoc, SpanStyle};

fn main() {
    let doc = RichDoc::new()
        .style("n", SpanStyle::new().size(17.0))
        .style("i", SpanStyle::new().size(17.0).italic())
        .style("b", SpanStyle::new().size(17.0).bold())
        .style("bi", SpanStyle::new().size(17.0).bold().italic())
        .para(Para::new().styled("n", "Normal 正常 handwriting"))
        .para(Para::new().styled("i", "Italic 斜体 handwriting"))
        .para(Para::new().styled("b", "Bold 粗体 handwriting"))
        .para(Para::new().styled("bi", "BoldItalic 粗斜 handwriting"))
        .para(
            Para::new()
                .styled("n", "混排：")
                .styled("i", "a juicy apple")
                .styled("n", " 与 ")
                .styled("b", "noun"),
        );
    App::new("italic", 560, 300)
        .screenshot_from_args()
        .content(
            Element::col()
                .padding(24)
                .spacing(8)
                .child(Element::label("Element::italic() →").font_size(15.0))
                .child(Element::label("倾斜的普通标签").font_size(17.0).italic())
                .child(Element::divider())
                .child(Element::rich(doc)),
        )
        .run();
}
