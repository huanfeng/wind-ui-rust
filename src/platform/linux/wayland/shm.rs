//! `wl_shm` 呈现缓冲：簿记（纯逻辑、可单测）与协议对象。
//!
//! 缓冲以 memfd 为底、用 `pwrite` 写入而**不在本进程映射**——合成器映射同一个 fd 读取，
//! 本进程的私有内存里只有宿主的 `Pixmap` 那一份。每窗最多两块缓冲：合成器还占着（未
//! `release`）的那块不能写，拿不到空闲缓冲时脏区留着，等 `release` 事件再呈现。每块缓冲
//! 记着「自己上次写入以来别的帧改过哪里」，复用时补写这部分，而不是整窗重写。
//!
//! memfd 走 glibc 的 `memfd_create`（2.27 起才有，见 `sys::memfd`）：更老的 glibc 上进程
//! 起不来，而那些系统（2018 年以前）本就没有可用的 Wayland 桌面，不单独做 `syscall` 回退。

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use tiny_skia::Pixmap;
use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::QueueHandle;

use super::super::host;
use super::Wl;
use crate::geometry::Rect;

/// 上屏转换缓冲的上限（字节）：整窗帧分块转换、分块写入，峰值不随窗口变大。
const UPLOAD_CHUNK: usize = 256 * 1024;
/// 每窗的 `wl_shm` 缓冲数。两块足够：一块在合成器手里，一块给下一帧写。
const MAX_BUFFERS: usize = 2;

/// 一帧的脏区，按缓冲里的两段分开记：上面的客户端标题栏、下面的内容（都是缓冲坐标）。
///
/// 分开是因为两段在缓冲的两头：标题栏按钮悬停（顶上一小块）与内容里的光标闪烁（中间一小块）
/// 并成一个包围盒就成了近乎整窗的上传。没有标题栏时 `bar` 恒为空。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Damage {
    pub bar: Option<Rect>,
    pub content: Option<Rect>,
}

impl Damage {
    pub fn bar(r: Rect) -> Self {
        Self {
            bar: Some(r),
            content: None,
        }
    }

    pub fn content(r: Rect) -> Self {
        Self {
            bar: None,
            content: Some(r),
        }
    }

    /// 整块：标题栏 `bar` 行 + 其下内容。
    pub fn full(w: i32, h: i32, bar: i32) -> Self {
        Self {
            bar: (bar > 0).then(|| Rect::new(0, 0, w, bar)),
            content: Some(Rect::new(0, bar, w, h - bar)),
        }
    }

    pub fn union(self, o: Damage) -> Self {
        let u = |a: Option<Rect>, b: Option<Rect>| match (a, b) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        };
        Self {
            bar: u(self.bar, o.bar),
            content: u(self.content, o.content),
        }
    }

    /// 裁到各自那一段里（尺寸变了、或调用方给的矩形越界时）。
    pub fn clip(self, w: i32, h: i32, bar: i32) -> Self {
        let f = Self::full(w, h, bar);
        let c = |a: Option<Rect>, full: Option<Rect>| {
            a.zip(full)
                .map(|(a, f)| a.intersect(&f))
                .filter(|r| !r.is_empty())
        };
        Self {
            bar: c(self.bar, f.bar),
            content: c(self.content, f.content),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rects().next().is_none()
    }

    pub fn rects(&self) -> impl Iterator<Item = Rect> + '_ {
        [self.bar, self.content]
            .into_iter()
            .flatten()
            .filter(|r| !r.is_empty())
    }
}

/// 一块缓冲的簿记。
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SlotState {
    w: i32,
    h: i32,
    /// 已 attach 给合成器、尚未收到 `release`：不能写。
    busy: bool,
    /// 自己上次写入以来，别的帧改过的区域——复用时要补写。
    debt: Damage,
    /// 建这块时的标题栏高度：变了等同尺寸变了（分段不同）。
    bar: i32,
}

/// 一次呈现的安排：写哪块缓冲、是否要按新尺寸重建、写哪个矩形。
#[derive(Debug, PartialEq)]
pub(super) struct SlotPlan {
    pub index: usize,
    pub recreate: bool,
    pub write: Damage,
}

#[derive(Default)]
pub(super) struct ShmSlots {
    slots: Vec<SlotState>,
}

impl ShmSlots {
    /// 为一帧（尺寸 `w×h`，相对上次呈现改了 `damage`）挑一块缓冲。都被合成器占着时返回
    /// `None`，调用方留着脏区，等 `release` 再来。
    ///
    /// 优先复用同尺寸的空闲块（只写 `damage ∪ debt`）；其次重建尺寸不对的空闲块；
    /// 不足 `MAX_BUFFERS` 块时新建。选中后标忙，其余块把本帧脏区记进欠账。
    /// `bar`：缓冲最上面的客户端标题栏行数（没有为 0），脏区与欠账按它分两段记。
    pub fn plan(&mut self, w: i32, h: i32, bar: i32, damage: Damage) -> Option<SlotPlan> {
        let full = Damage::full(w, h, bar);
        let free_same = self
            .slots
            .iter()
            .position(|s| !s.busy && s.w == w && s.h == h && s.bar == bar);
        let plan = if let Some(index) = free_same {
            let debt = self.slots[index].debt;
            let write = debt.union(damage).clip(w, h, bar);
            SlotPlan {
                index,
                recreate: false,
                write,
            }
        } else if let Some(index) = self.slots.iter().position(|s| !s.busy) {
            SlotPlan {
                index,
                recreate: true,
                write: full,
            }
        } else if self.slots.len() < MAX_BUFFERS {
            self.slots.push(SlotState {
                w,
                h,
                busy: false,
                debt: Damage::default(),
                bar,
            });
            SlotPlan {
                index: self.slots.len() - 1,
                recreate: true,
                write: full,
            }
        } else {
            return None;
        };
        for (i, s) in self.slots.iter_mut().enumerate() {
            if i == plan.index {
                *s = SlotState {
                    w,
                    h,
                    busy: true,
                    debt: Damage::default(),
                    bar,
                };
            } else {
                s.debt = s.debt.union(damage);
            }
        }
        Some(plan)
    }

    pub fn release(&mut self, index: usize) {
        if let Some(s) = self.slots.get_mut(index) {
            s.busy = false;
        }
    }

    /// `plan` 之后建缓冲或写像素失败：这块的内容不可信，作废到「下次必须按新建处理、
    /// 整块重写」。只 `release` 的话它会被当成已写好，下次只补一小块脏区。
    pub fn fail(&mut self, index: usize) {
        if let Some(s) = self.slots.get_mut(index) {
            *s = SlotState {
                w: 0,
                h: 0,
                busy: false,
                debt: Damage::default(),
                bar: 0,
            };
        }
    }
}

/// 一块缓冲的协议对象。memfd 留着给 `pwrite`；池在建完缓冲后即销毁（缓冲自己持有映射）。
pub(super) struct ShmBuffer {
    pub file: File,
    pub buffer: wl_buffer::WlBuffer,
}

impl ShmBuffer {
    pub fn destroy(self) {
        self.buffer.destroy();
    }
}

pub(super) fn create_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<Wl>,
    key: u32,
    slot: usize,
    w: i32,
    h: i32,
) -> std::io::Result<ShmBuffer> {
    let stride = w * 4;
    let size = i32::try_from(stride as i64 * h as i64)
        .map_err(|_| std::io::Error::other(format!("缓冲过大（{w}×{h}）")))?;
    let file = super::super::sys::memfd(c"windui-shm", size as u64)?;
    let pool = shm.create_pool(file.as_fd(), size, qh, ());
    let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Xrgb8888, qh, (key, slot));
    // 池只是建缓冲的中介：缓冲自己持有映射，销毁池不影响它。
    pool.destroy();
    Ok(ShmBuffer { file, buffer })
}

/// 把 pixmap 的 `r` 区域换成 XRGB8888 写进缓冲文件（同尺寸、行距 = 宽 × 4）。
/// 整行宽的区域按块连续写，否则逐行写。
/// `dy`：写到缓冲里下移多少行（缓冲与 `pm` 同宽；客户端标题栏占缓冲最上面几行时，内容整体
/// 下移标题栏高度）。
pub(super) fn write_pixels(
    file: &File,
    pm: &Pixmap,
    r: Rect,
    dy: i32,
    buf: &mut Vec<u8>,
) -> std::io::Result<()> {
    let r = r.intersect(&Rect::new(0, 0, pm.width() as i32, pm.height() as i32));
    if r.is_empty() {
        return Ok(());
    }
    let stride = pm.width() as usize * 4;
    let data = pm.data();
    let row_bytes = r.w as usize * 4;
    let full_rows = row_bytes == stride;
    let rows_per = if full_rows {
        (UPLOAD_CHUNK / row_bytes).max(1)
    } else {
        1
    };
    let mut y = r.y as usize;
    let bottom = r.bottom() as usize;
    while y < bottom {
        let n = rows_per.min(bottom - y);
        buf.clear();
        for row in y..y + n {
            let off = row * stride + r.x as usize * 4;
            host::rgba_to_bgra(&data[off..off + row_bytes], buf);
        }
        file.write_all_at(buf, ((y + dy as usize) * stride + r.x as usize * 4) as u64)?;
        y += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn first_frames_create_two_buffers_then_wait_for_release() {
        let mut p = ShmSlots::default();
        let a = p
            .plan(100, 50, 0, Damage::content(r(0, 0, 100, 50)))
            .unwrap();
        assert_eq!(
            (a.index, a.recreate, a.write.content),
            (0, true, Some(r(0, 0, 100, 50)))
        );
        let b = p
            .plan(100, 50, 0, Damage::content(r(10, 10, 5, 5)))
            .unwrap();
        assert_eq!(
            (b.index, b.recreate),
            (1, true),
            "第一块还在合成器手里，新建第二块"
        );
        assert_eq!(b.write.content, Some(r(0, 0, 100, 50)), "新缓冲整块写");
        assert_eq!(
            p.plan(100, 50, 0, Damage::content(r(0, 0, 1, 1))),
            None,
            "两块都忙：等 release"
        );
    }

    #[test]
    fn reused_buffer_writes_damage_plus_what_it_missed() {
        let mut p = ShmSlots::default();
        p.plan(100, 50, 0, Damage::content(r(0, 0, 100, 50)))
            .unwrap(); // 0
        p.plan(100, 50, 0, Damage::content(r(10, 10, 5, 5)))
            .unwrap(); // 1，0 欠 (10,10,5,5)
        p.release(0);
        let c = p
            .plan(100, 50, 0, Damage::content(r(40, 20, 2, 2)))
            .unwrap();
        assert_eq!(c.index, 0);
        assert!(!c.recreate);
        assert_eq!(
            c.write.content,
            Some(r(10, 10, 5, 5).union(&r(40, 20, 2, 2)))
        );
        p.release(1);
        let d = p.plan(100, 50, 0, Damage::content(r(0, 0, 1, 1))).unwrap();
        assert_eq!(d.index, 1);
        assert_eq!(
            d.write.content,
            Some(r(40, 20, 2, 2).union(&r(0, 0, 1, 1))),
            "欠账在自己被写之后清零，只剩之后别的帧的脏区"
        );
    }

    #[test]
    fn resize_recreates_free_buffers_and_keeps_busy_ones() {
        let mut p = ShmSlots::default();
        p.plan(100, 50, 0, Damage::content(r(0, 0, 100, 50)))
            .unwrap(); // 0 忙
        p.plan(100, 50, 0, Damage::content(r(0, 0, 100, 50)))
            .unwrap(); // 1 忙
        p.release(1);
        let e = p
            .plan(120, 60, 0, Damage::content(r(0, 0, 120, 60)))
            .unwrap();
        assert_eq!(
            (e.index, e.recreate, e.write.content),
            (1, true, Some(r(0, 0, 120, 60)))
        );
        p.release(0);
        let f = p.plan(120, 60, 0, Damage::content(r(5, 5, 1, 1))).unwrap();
        assert_eq!(
            (f.index, f.recreate),
            (0, true),
            "旧尺寸的块空出来后按新尺寸重建"
        );
        assert_eq!(f.write.content, Some(r(0, 0, 120, 60)));
    }

    #[test]
    fn failed_slot_is_rebuilt_and_fully_rewritten() {
        let mut p = ShmSlots::default();
        p.plan(100, 50, 0, Damage::content(r(0, 0, 100, 50)))
            .unwrap();
        p.fail(0);
        let h = p.plan(100, 50, 0, Damage::content(r(1, 1, 1, 1))).unwrap();
        assert_eq!(
            (h.index, h.recreate, h.write.content),
            (0, true, Some(r(0, 0, 100, 50)))
        );
    }

    #[test]
    fn titlebar_and_content_damage_stay_separate_instead_of_one_bounding_box() {
        let mut p = ShmSlots::default();
        // 600×433，标题栏 33 行。
        p.plan(600, 433, 33, Damage::full(600, 433, 33)).unwrap(); // 0
        p.release(0);
        // 标题栏按钮悬停（顶上）+ 内容里光标闪烁（中间）。
        let dmg = Damage {
            bar: Some(r(500, 0, 50, 33)),
            content: Some(r(20, 200, 2, 17)),
        };
        let w = p.plan(600, 433, 33, dmg).unwrap().write;
        assert_eq!(w, dmg, "两段各写各的，不并成从顶到中间的大块");
        let area: i32 = w.rects().map(|r| r.w * r.h).sum();
        assert!(area < 2000, "上传面积 {area}");
        // 第二块新建（整块）；之后第一块复用时，欠账同样分段记。
        p.plan(600, 433, 33, Damage::bar(r(0, 0, 1, 1))).unwrap(); // 1，0 欠一个标题栏像素
        p.release(0);
        let w = p
            .plan(600, 433, 33, Damage::content(r(10, 300, 1, 1)))
            .unwrap()
            .write;
        assert_eq!(
            w,
            Damage {
                bar: Some(r(0, 0, 1, 1)),
                content: Some(r(10, 300, 1, 1)),
            }
        );
    }

    #[test]
    fn a_titlebar_height_change_rebuilds_the_buffer() {
        let mut p = ShmSlots::default();
        p.plan(600, 400, 0, Damage::full(600, 400, 0)).unwrap();
        p.release(0);
        let q = p
            .plan(600, 400, 33, Damage::content(r(0, 40, 1, 1)))
            .unwrap();
        assert!(q.recreate, "分段变了：整块重写");
        assert_eq!(q.write, Damage::full(600, 400, 33));
    }

    #[test]
    fn content_is_shifted_down_below_the_titlebar_rows() {
        // 缓冲 4×5：上 2 行是标题栏，内容 pixmap 4×3 整体下移 2 行。
        let mut content = Pixmap::new(4, 3).unwrap();
        content.fill(tiny_skia::Color::from_rgba8(10, 20, 30, 255));
        let mut bar = Pixmap::new(4, 2).unwrap();
        bar.fill(tiny_skia::Color::from_rgba8(200, 100, 50, 255));
        let file = super::super::super::sys::memfd(c"windui-test", 4 * 5 * 4).unwrap();
        let mut buf = Vec::new();
        write_pixels(&file, &content, r(0, 0, 4, 3), 2, &mut buf).unwrap();
        let mut out = vec![0u8; 80];
        file.read_exact_at(&mut out, 0).unwrap();
        let px =
            |out: &[u8], x: usize, y: usize| out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4].to_vec();
        assert_eq!(px(&out, 0, 0), [0, 0, 0, 0], "标题栏那两行没被内容写到");
        assert_eq!(px(&out, 3, 1), [0, 0, 0, 0]);
        assert_eq!(
            px(&out, 0, 2),
            [30, 20, 10, 255],
            "内容第 0 行落在缓冲第 2 行"
        );
        assert_eq!(px(&out, 3, 4), [30, 20, 10, 255]);
        write_pixels(&file, &bar, r(0, 0, 4, 2), 0, &mut buf).unwrap();
        file.read_exact_at(&mut out, 0).unwrap();
        assert_eq!(px(&out, 2, 1), [50, 100, 200, 255], "标题栏写在最上面");
        assert_eq!(px(&out, 2, 2), [30, 20, 10, 255], "没盖到内容");
    }

    #[test]
    fn pixels_land_in_xrgb_layout_at_the_right_offset() {
        let mut pm = Pixmap::new(4, 3).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(10, 20, 30, 255));
        let file = super::super::super::sys::memfd(c"windui-test", 4 * 3 * 4).unwrap();
        let mut buf = Vec::new();
        // 非整行：只写 (1,1) 一个像素。
        write_pixels(&file, &pm, r(1, 1, 1, 1), 0, &mut buf).unwrap();
        let mut out = vec![0u8; 48];
        file.read_exact_at(&mut out, 0).unwrap();
        fn px(out: &[u8], x: usize, y: usize) -> &[u8] {
            &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4]
        }
        assert_eq!(px(&out, 1, 1), [30, 20, 10, 255]);
        assert_eq!(px(&out, 0, 0), [0, 0, 0, 0], "区域外不写");
        // 整行：第 2 行整行连续写。
        write_pixels(&file, &pm, r(0, 2, 4, 1), 0, &mut buf).unwrap();
        file.read_exact_at(&mut out, 0).unwrap();
        assert_eq!(px(&out, 3, 2), [30, 20, 10, 255]);
        assert_eq!(px(&out, 0, 1), [0, 0, 0, 0]);
    }
}
