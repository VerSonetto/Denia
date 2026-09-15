//! 二维码渲染:同一份编码结果出三种形态。
//!
//! - `svg` 给前端 `<img>`/内联展示(矢量,扫码最稳);
//! - `ascii` 给终端(用户可以在没有前端的环境里直接扫屏幕);
//! - `matrix` 给前端 Canvas 画 PNG(手机端"长按保存到相册")。
//!
//! 三种形态共用一次编码,避免三处各自纠错级别导致"扫得出来/扫不出来"的
//! 玄学差异。

use qrcode::QrCode;
use qrcode::render::svg;
use qrcode::render::unicode::Dense1x2;

/// 纠错级别 M:约 15% 冗余。手机扫屏幕时对反光/摩尔纹有一定容忍度,
/// 又不至于把版本数推高到二维码密密麻麻。
const EC_LEVEL: qrcode::EcLevel = qrcode::EcLevel::M;

/// 一份编码好的二维码。
#[derive(Debug, Clone)]
pub struct Rendered {
    pub svg: String,
    pub ascii: String,
    pub width: usize,
    /// 行优先的模块矩阵:`true` = 深色。不含静默区。
    pub modules: Vec<bool>,
}

/// 编码失败(内容过长等)返回可读原因,不 panic。
pub fn render(content: &str) -> Result<Rendered, String> {
    let code = QrCode::with_error_correction_level(content.as_bytes(), EC_LEVEL)
        .map_err(|e| format!("二维码编码失败:{e}"))?;
    let width = code.width();
    let modules: Vec<bool> = code
        .to_colors()
        .into_iter()
        .map(|color| color == qrcode::Color::Dark)
        .collect();
    debug_assert_eq!(modules.len(), width * width);

    let svg = code
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build();

    let ascii = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Dark)
        .light_color(Dense1x2::Light)
        .build();

    Ok(Rendered {
        svg,
        ascii,
        width,
        modules,
    })
}

/// 终端展示用的带框 ASCII 二维码:上下各一行空白做静默区。
///
/// 左右静默区由 qrcode 的 `unicode` 渲染自带(它按 2 行合 1 行输出,
/// 横向静默区已在编码里)。这里只补上下边框,并统一缩进便于对齐。
pub fn terminal_block(content: &str) -> Result<String, String> {
    let rendered = render(content)?;
    let mut out = String::with_capacity(rendered.ascii.len() + 64);
    out.push('\n');
    for line in rendered.ascii.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "http://192.168.1.10:3602/?ticket=abcdefghijklmnopqrstuvwxyz0123456789ABC";

    #[test]
    fn renders_all_three_forms() {
        let rendered = render(SAMPLE).unwrap();
        assert!(rendered.width >= 21, "最小版本宽度是 21,得到 {}", rendered.width);
        assert_eq!(rendered.modules.len(), rendered.width * rendered.width);
        assert!(rendered.svg.starts_with("<?xml"), "SVG 头不对");
        assert!(rendered.svg.contains("<svg"));
        assert!(rendered.svg.contains("viewBox"));
        // 正方形:宽高必须一致,否则扫码器会因拉伸失焦。
        let width = svg_attr(&rendered.svg, "width");
        let height = svg_attr(&rendered.svg, "height");
        assert_eq!(width, height, "SVG 必须正方形");
        assert!(width >= 240, "SVG 尺寸应当不小于请求的最小边长,得到 {width}");
    }

    /// 从 SVG 根元素取一个数值属性。
    fn svg_attr(svg: &str, name: &str) -> u32 {
        let needle = format!("{name}=\"");
        let start = svg.find(&needle).expect("属性存在") + needle.len();
        let rest = &svg[start..];
        let end = rest.find('"').expect("属性闭合");
        rest[..end].parse().expect("数值属性")
    }

    #[test]
    fn ascii_uses_half_block_characters_only() {
        let rendered = render(SAMPLE).unwrap();
        for line in rendered.ascii.lines() {
            for ch in line.chars() {
                assert!(
                    matches!(ch, ' ' | '\u{2580}' | '\u{2584}' | '\u{2588}'),
                    "ASCII 二维码出现了意外字符 {ch:?}"
                );
            }
        }
    }

    #[test]
    fn ascii_row_count_matches_module_rows() {
        let rendered = render(SAMPLE).unwrap();
        // unicode 渲染把 2 行模块合成 1 行字符,并带 4 模块静默区(渲染器默认)。
        // 因此字符行数 = ceil((宽度 + 8) / 2)。
        let expected = (rendered.width + 8).div_ceil(2);
        assert_eq!(rendered.ascii.lines().count(), expected);
    }

    #[test]
    fn matrix_matches_encoded_width_without_quiet_zone() {
        let rendered = render(SAMPLE).unwrap();
        // 矩阵给前端 Canvas 画 PNG:它自己补静默区,所以这里不带。
        assert_eq!(rendered.modules.len(), rendered.width * rendered.width);
        // 二维码左上角必须是定位图案的深色模块 —— 拿它校验矩阵朝向没反。
        assert!(rendered.modules[0], "左上角定位图案应当是深色");
        // 四角定位图案存在:第一行第 7 列是深色(标准 QR 定位图案形状)。
        assert!(rendered.modules[6]);
    }

    #[test]
    fn terminal_block_is_indented_and_framed() {
        let block = terminal_block(SAMPLE).unwrap();
        assert!(block.starts_with('\n'));
        assert!(block.ends_with("\n\n"));
        assert!(block.lines().skip(1).all(|line| line.is_empty() || line.starts_with("  ")));
    }

    #[test]
    fn overlong_content_fails_loud() {
        let huge = "x".repeat(5000);
        assert!(render(&huge).is_err(), "超长内容必须报错而不是 panic");
    }
}
