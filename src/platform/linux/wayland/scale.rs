//! HiDPI 的纯换算（可单测）：取哪一种缩放、逻辑 ↔ 物理像素怎么取整。
//!
//! 两条路：
//! - **整数缩放**：`wl_surface.set_buffer_scale(n)`，缓冲 = 逻辑尺寸 × n，合成器按 n 缩回。
//!   所有合成器都支持；缩放因子来自 `preferred_buffer_scale`（`wl_surface` v6）或表面所在
//!   输出的 `wl_output.scale`。
//! - **分数缩放**：`fractional-scale-v1` 报首选缩放（×120 的整数），缓冲按
//!   `round(逻辑 × s)` 画，再由 `viewporter` 把缓冲钉到逻辑尺寸（buffer_scale 保持 1）。
//!   两个协议缺一不可。
//!
//! 取整规则只有一条：物理尺寸 = `round(逻辑 × s)`（远离零取整，协议原文要求），指针坐标
//! 同样 `round(表面坐标 × s)`。整窗只有一个缓冲、只做一次取整，不存在拼接缝。

/// 缩放方式与因子。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Scale {
    pub factor: f64,
    /// true = 走 viewport（缓冲按分数缩放画、buffer_scale = 1）；false = buffer_scale = factor。
    pub viewport: bool,
}

impl Scale {
    #[cfg(test)]
    pub const ONE: Scale = Scale {
        factor: 1.0,
        viewport: false,
    };

    /// `set_buffer_scale` 该传的值：viewport 模式下恒为 1。
    pub fn buffer_scale(self) -> i32 {
        if self.viewport {
            1
        } else {
            self.factor as i32
        }
    }

    /// 逻辑长度 → 物理像素（至少 1）。
    pub fn to_physical(self, logical: i32) -> i32 {
        ((logical as f64 * self.factor).round() as i32).max(1)
    }

    /// 表面坐标（逻辑，可带小数）→ 物理像素坐标。
    pub fn pos_to_physical(self, v: f64) -> i32 {
        (v * self.factor).round() as i32
    }
}

/// 缩放来源，按优先级从高到低。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Sources {
    /// `WINDUI_SCALE`（强制，可为分数）。
    pub forced: Option<f64>,
    /// `wp_fractional_scale_v1.preferred_scale`（×120）。
    pub fractional120: Option<u32>,
    /// `wl_surface.preferred_buffer_scale`（v6）。
    pub preferred_int: Option<i32>,
    /// 表面所在各输出 `wl_output.scale` 的最大值（跨两块屏时取大的，免得在高分屏那半边发糊）。
    pub outputs_max: Option<i32>,
    /// 合成器有 `wp_viewporter`（分数缩放的前提）。
    pub viewporter: bool,
}

/// 选定缩放。分数值只在有 viewporter 时才能按分数画，否则就近取整走 buffer_scale。
pub(super) fn pick(src: Sources) -> Scale {
    let frac = |f: f64| {
        let f = f.clamp(0.5, 4.0);
        let n = f.round().max(1.0);
        if (f - n).abs() < 1e-6 {
            Scale {
                factor: n,
                viewport: false,
            }
        } else if src.viewporter {
            Scale {
                factor: f,
                viewport: true,
            }
        } else {
            Scale {
                factor: n,
                viewport: false,
            }
        }
    };
    if let Some(f) = src.forced.filter(|f| f.is_finite() && *f > 0.0) {
        return frac(f);
    }
    if let Some(v) = src.fractional120.filter(|v| *v > 0) {
        if src.viewporter {
            return frac(v as f64 / 120.0);
        }
    }
    let int = src
        .preferred_int
        .or(src.outputs_max)
        .unwrap_or(1)
        .clamp(1, 4);
    Scale {
        factor: int as f64,
        viewport: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_forced_then_fractional_then_integer_sources() {
        let base = Sources {
            fractional120: Some(180),
            preferred_int: Some(2),
            outputs_max: Some(3),
            viewporter: true,
            ..Sources::default()
        };
        assert_eq!(
            pick(Sources {
                forced: Some(1.25),
                ..base
            }),
            Scale {
                factor: 1.25,
                viewport: true
            }
        );
        assert_eq!(pick(base).factor, 1.5, "有 viewporter 时分数缩放优先");
        let no_frac = Sources {
            fractional120: None,
            ..base
        };
        assert_eq!(
            pick(no_frac),
            Scale {
                factor: 2.0,
                viewport: false
            }
        );
        let only_outputs = Sources {
            preferred_int: None,
            ..no_frac
        };
        assert_eq!(pick(only_outputs).factor, 3.0);
        assert_eq!(pick(Sources::default()), Scale::ONE);
    }

    #[test]
    fn fractional_needs_viewporter_else_rounds() {
        let s = pick(Sources {
            fractional120: Some(150),
            preferred_int: Some(1),
            ..Sources::default()
        });
        assert_eq!(
            s,
            Scale::ONE,
            "没有 viewporter：分数缩放不可用，退回整数来源"
        );
        let s = pick(Sources {
            forced: Some(1.5),
            ..Sources::default()
        });
        assert_eq!(
            s,
            Scale {
                factor: 2.0,
                viewport: false
            },
            "强制分数也只能就近取整"
        );
    }

    #[test]
    fn integral_fraction_uses_buffer_scale() {
        let s = pick(Sources {
            fractional120: Some(240),
            viewporter: true,
            ..Sources::default()
        });
        assert_eq!(
            s,
            Scale {
                factor: 2.0,
                viewport: false
            }
        );
        assert_eq!(s.buffer_scale(), 2);
    }

    #[test]
    fn physical_rounding_follows_protocol() {
        let s = Scale {
            factor: 1.25,
            viewport: true,
        };
        assert_eq!(s.to_physical(601), 751, "751.25 → 751");
        assert_eq!(s.to_physical(602), 753, "752.5 远离零取整 → 753");
        assert_eq!(s.buffer_scale(), 1);
        assert_eq!(s.pos_to_physical(10.4), 13);
        assert_eq!(Scale::ONE.to_physical(0), 1, "至少 1 像素");
        let s2 = Scale {
            factor: 2.0,
            viewport: false,
        };
        assert_eq!(
            s2.to_physical(301) % 2,
            0,
            "整数缩放下缓冲恒为 buffer_scale 的整数倍"
        );
    }

    #[test]
    fn garbage_forced_values_are_ignored() {
        let s = pick(Sources {
            forced: Some(f64::NAN),
            preferred_int: Some(2),
            ..Sources::default()
        });
        assert_eq!(s.factor, 2.0);
    }
}
