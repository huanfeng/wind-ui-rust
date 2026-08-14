//! 上下文菜单浮层：宿主层自绘的级联面板。
//!
//! 完全自洽的一块：自己的状态（面板栈、滚动、悬停）、自己的命中逻辑（不走控件树的
//! Enter/Leave）、自己的绘制。与 `Tree` 的唯一往来是叶子项激活时向目标控件合成按键
//! 或借它的 `EventCtx` 跑闭包。

use std::rc::Rc;

use crate::core::NodeId;
use crate::event::{Key, MenuAction, MenuItem, PointerEvent, PointerKind};
use crate::geometry::{Point, Rect};
use crate::render::{Canvas, Paint};
use crate::text::{TextEngine, TextStyle};
use crate::theme::Theme;

use super::focus::FocusSource;
use super::UiHost;

pub(super) const MENU_ITEM_H: i32 = 30;
/// 两行项（带 subtitle）行高。
const MENU_ITEM_H_TALL: i32 = 46;
const MENU_SEP_H: i32 = 9;
pub(super) const MENU_PAD_X: i32 = 12;
pub(super) const MENU_VPAD: i32 = 6;
const MENU_MIN_W: i32 = 140;
/// 滚动条命中区宽度。刻意宽于可见滑块（5px）：细滑块难点中，留出横向容错。
/// 纵向不留容错——见 [`MenuLevel::scrollbar_geom`]。
const SCROLLBAR_HIT_W: i32 = 16;
/// 下拉菜单面板最大可视高度（超出后启用滚动）。
const MENU_MAX_H: i32 = 320;
const MENU_FONT: f32 = 13.5;
/// 图标列宽（有图标项时预留），也用作尾随可点击图标的命中/绘制列宽。
pub(super) const MENU_ICON_W: i32 = 18;
/// 图标与标签间距。
const MENU_GAP: i32 = 8;
/// 标签与尾随（快捷键/箭头）间最小间距。
const MENU_TRAIL_GAP: i32 = 18;
/// 尾随徽章胶囊左右内边距。
const BADGE_PAD_X: i32 = 8;
/// 尾随徽章胶囊高度。
const BADGE_H: i32 = 20;
/// 菜单弹层距窗口四边最小留白（逻辑像素）：与 resize 边框区域宽度对齐，
/// 确保弹层滚动条不会覆盖到缩放操作区域，无需修改 WM_NCHITTEST 优先级。
const MENU_EDGE_MARGIN: i32 = 10;

/// 单项行高：分隔线固定细线高；带 subtitle 的项两行更高；否则单行标准高。
fn menu_item_height(it: &MenuItem) -> i32 {
    if it.separator {
        MENU_SEP_H
    } else if it.subtitle.is_some() {
        MENU_ITEM_H_TALL
    } else {
        MENU_ITEM_H
    }
}

/// 规范化分组分隔线：去掉首尾的、把相邻多条折叠成一条。
///
/// 菜单项**按条件生成**是常态（某一整组可能一项都不剩），而分组分隔线只有无条件写下来
/// 才读得顺。若不在这里收，每个调用方都得自己回溯"上一项是不是分隔线"——写起来啰嗦，
/// 且必然有漏网的分支，症状就是菜单里凭空多出一条线、或者顶上/底下悬着一条。
///
/// 在 `build_level`（所有层的唯一构建入口）与 `refresh_items` 各调一次，故子菜单同样受益。
fn normalize_separators(items: &mut Vec<MenuItem>) {
    items.dedup_by(|a, b| a.separator && b.separator);
    if items.first().is_some_and(|i| i.separator) {
        items.remove(0);
    }
    if items.last().is_some_and(|i| i.separator) {
        items.pop();
    }
}

/// 该项能否被键盘高亮停留：分隔线与禁用项跳过。子菜单父项**可以**停留
/// （停在它上面按 → 展开），故不看 `submenu`——这与 `MenuItem::is_actionable`
/// （能否执行）是两个问题。
fn menu_item_selectable(it: &MenuItem) -> bool {
    !it.separator && it.enabled
}

/// 单级菜单面板：一组项 + 面板矩形 + 悬停项 + 是否含图标列。
pub(super) struct MenuLevel {
    pub(super) items: Vec<MenuItem>,
    pub(super) rect: Rect,
    pub(super) hover: Option<usize>,
    pub(super) has_icons: bool,
    /// 该级由父级哪一项展开（根级为 None）；用于避免同项重复重建子菜单。
    pub(super) spawn: Option<usize>,
    /// 项内容总高（含上下内边距，未截断）；超出 rect.h 时启用滚动。
    pub(super) content_h: i32,
    /// 当前滚动偏移（像素，0=顶部）。
    pub(super) scroll: i32,
}

impl MenuLevel {
    /// 每项的 (顶部 y, 高度)（逻辑坐标，已减去 scroll 偏移）。
    fn item_rows(&self) -> Vec<(i32, i32)> {
        let mut y = self.rect.y + MENU_VPAD - self.scroll;
        let mut rows = Vec::with_capacity(self.items.len());
        for it in &self.items {
            let h = menu_item_height(it);
            rows.push((y, h));
            y += h;
        }
        rows
    }
    /// 最大可滚动量（content_h 超出面板高时才有效）。
    fn max_scroll(&self) -> i32 {
        (self.content_h - self.rect.h).max(0)
    }
    /// 条目的**可视带**：面板矩形上下各内缩 `MENU_VPAD`，使条目在触达圆角边框前
    /// 自然裁切。绘制的 `clip_rect` 与所有条目命中判据都取自这里——两边各写一份
    /// 就会分叉：滚动后挪进边带的行被裁掉看不见，却仍然可点，用户点到的是一个
    /// 不存在于屏幕上的项。
    pub(super) fn item_clip(&self) -> Rect {
        Rect::new(
            self.rect.x,
            self.rect.y + MENU_VPAD,
            self.rect.w,
            (self.rect.h - 2 * MENU_VPAD).max(0),
        )
    }
    /// 滚动条的**轨道与滑块几何** `(轨道矩形, 滑块 y, 滑块高)`；内容未超高时为 `None`。
    ///
    /// 与 [`MenuLevel::item_clip`] 同一个用意：绘制与命中共取一处，免得分叉。此前
    /// 命中判据只写了 `x >= right - 16`、完全不约束 y，而滑块只画在 `y+4` 起、高
    /// `h-8` 的轨道内——点面板右缘顶/底那几像素会开始拖一个视觉上不在那里的滑块。
    fn scrollbar_geom(&self) -> Option<(Rect, f32, f32)> {
        let r = self.rect;
        if self.content_h <= r.h {
            return None;
        }
        // 命中区（16px）比可见滑块（5px）宽，是有意的：细滑块难点中，留出容错。
        // 但纵向必须与轨道一致，否则就是"看不见却可拖"。
        let track = Rect::new(
            r.right() - SCROLLBAR_HIT_W,
            r.y + 4,
            SCROLLBAR_HIT_W,
            r.h - 8,
        );
        let ratio = r.h as f32 / self.content_h as f32;
        let thumb_h = (track.h as f32 * ratio).max(20.0);
        let max_sc = self.max_scroll().max(1) as f32;
        let thumb_y = track.y as f32 + (track.h as f32 - thumb_h) * (self.scroll as f32 / max_sc);
        Some((track, thumb_y, thumb_h))
    }
    /// 命中点 → 项下标（分隔线不可命中）。
    fn item_at(&self, p: Point) -> Option<usize> {
        if !self.item_clip().contains(p) {
            return None;
        }
        for (i, (top, h)) in self.item_rows().into_iter().enumerate() {
            if p.y >= top && p.y < top + h {
                return if self.items[i].separator {
                    None
                } else {
                    Some(i)
                };
            }
        }
        None
    }
    /// 命中尾随可点击图标 → 项下标。图标固定贴右绘制（`r.right()-MENU_PAD_X-MENU_ICON_W`
    /// 起始），与 badge 是否存在无关，故命中矩形无需重算 badge 宽度即可复刻绘制位置。
    pub(super) fn trailing_icon_at(&self, p: Point) -> Option<usize> {
        if !self.item_clip().contains(p) {
            return None;
        }
        let icon_left = self.rect.right() - MENU_PAD_X - MENU_ICON_W;
        let icon_right = self.rect.right() - MENU_PAD_X;
        if p.x < icon_left || p.x >= icon_right {
            return None;
        }
        for (i, (top, h)) in self.item_rows().into_iter().enumerate() {
            if p.y >= top && p.y < top + h && self.items[i].trailing_icon.is_some() {
                return Some(i);
            }
        }
        None
    }
}

/// 宿主管理的上下文菜单浮层：可级联多级面板，在控件树之上自绘、拦截指针，
/// 叶子项激活时向目标控件合成按键或运行闭包。
pub(super) struct ContextMenu {
    /// 面板栈：levels[0]=根，其后为依次展开的子菜单。
    pub(super) levels: Vec<MenuLevel>,
    /// 发起菜单的控件（合成按键的派发目标）。
    pub(super) target: NodeId,
    /// 项重建器（见 [`crate::event::MenuRequest::rebuild`]）：粘滞项点击后原地刷新。
    pub(super) rebuild: Option<Rc<dyn Fn() -> Vec<MenuItem>>>,
}

impl ContextMenu {
    /// 命中点落在最深（最上层）的哪一级面板内。
    fn level_at(&self, p: Point) -> Option<usize> {
        self.levels.iter().rposition(|l| l.rect.contains(p))
    }

    /// 粘滞项点击后原地刷新各级项：沿 `spawn` 路径把重建结果逐级换进去，
    /// **保留每级的 rect/scroll/hover**（见 `MenuRequest::rebuild` 关于宽度不变的说明）。
    /// 重建后项数变少导致某级的 spawn 越界或不再是子菜单父项时，截断其后的级。
    fn refresh_items(&mut self) {
        let Some(rb) = self.rebuild.clone() else {
            return;
        };
        let mut items = rb();
        // 必须在用下标取子菜单**之前**规范化：`spawn` 记的是已规范化列表里的位置
        //（`build_level` 先规范化再定下标），拿未规范化的列表按同一下标取会错位。
        normalize_separators(&mut items);
        let mut keep = self.levels.len();
        for k in 0..self.levels.len() {
            let next_spawn = self.levels.get(k + 1).and_then(|l| l.spawn);
            // 子级同样要规范化，理由同上：`spawn` 记的是规范化后列表里的位置。
            // `submenu` 字段存的始终是原始列表（`build_level` 只规范化本级、不动各项的
            // `submenu`），漏了这一步就只有根层干净——子菜单的孤立分隔线会在每次刷新后
            // 复现，且下标错位会把已展开的孙级菜单静默关掉。
            let sub = next_spawn.and_then(|i| items.get(i)).map(|it| {
                let mut s = it.submenu.clone();
                normalize_separators(&mut s);
                s
            });
            self.levels[k].has_icons = items.iter().any(|it| it.icon.is_some());
            self.levels[k].content_h =
                items.iter().map(menu_item_height).sum::<i32>() + 2 * MENU_VPAD;
            self.levels[k].items = items;
            let max_sc = self.levels[k].max_scroll();
            self.levels[k].scroll = self.levels[k].scroll.clamp(0, max_sc);
            match sub {
                // 子菜单父项仍在：继续把它的子项换进下一级。
                Some(s) if !s.is_empty() => items = s,
                // 下一级已无来源（项没了/不再有子菜单）：截断到本级。
                _ => {
                    keep = k + 1;
                    break;
                }
            }
        }
        self.levels.truncate(keep);
    }
}

/// 菜单滚动条鼠标拖拽状态。
struct MenuScrollbarDrag {
    /// 正在拖拽的面板层级下标。
    level: usize,
    /// 拖拽起始的鼠标 y（逻辑坐标）。
    start_y: i32,
    /// 拖拽起始时的 scroll 偏移。
    start_scroll: i32,
    /// 可滑动轨道高度（面板高 - 上下 padding）。
    track_h: f32,
    /// 拖拽起始时的滑块高度（同帧渲染几何）。
    thumb_h: f32,
}

/// 宿主持有的菜单浮层状态：活动菜单 + 滚动条拖拽。
#[derive(Default)]
pub(super) struct MenuHost {
    /// 活动的上下文菜单浮层（None=无）。
    pub(super) active: Option<ContextMenu>,
    /// 菜单滚动条拖拽状态（None=无）。
    scrollbar_drag: Option<MenuScrollbarDrag>,
}

impl MenuHost {
    /// 中止进行中的滚动条拖拽，返回此前是否正在拖。
    ///
    /// 供捕获丢失时收尾：这条拖拽不走 `UiHost::capture`（菜单打开时指针事件在进入
    /// 控件树之前就被浮层截走，逻辑捕获从未建立），所以 `on_capture_lost` 里对
    /// `capture` 的判空覆盖不到它——不单独收尾就会切走应用再回来时滑块还粘着指针。
    pub(super) fn abort_scrollbar_drag(&mut self) -> bool {
        self.scrollbar_drag.take().is_some()
    }
}

impl MenuHost {
    /// 当前是否有浮层菜单展开。
    pub(super) fn is_open(&self) -> bool {
        self.active.is_some()
    }

    /// 逻辑坐标是否落在任一级面板内（平台层用于把弹层区域判为客户区）。
    pub(super) fn hit_any_panel(&self, p: Point) -> bool {
        self.active
            .as_ref()
            .is_some_and(|m| m.levels.iter().any(|l| l.rect.contains(p)))
    }
}

impl UiHost {
    /// 测量一组菜单项所需面板宽度（图标列 + 标签 + 尾随快捷键/箭头）及是否含图标列。
    fn level_width(&mut self, items: &[MenuItem], min_width: i32) -> (i32, bool) {
        let has_icons = items.iter().any(|it| it.icon.is_some());
        let mut max_label = 0;
        let mut max_trail = 0;
        for it in items {
            if it.separator {
                continue;
            }
            max_label = max_label.max(
                self.engine
                    .measure(&it.label, &TextStyle::new(MENU_FONT), None)
                    .w,
            );
            if let Some(sub) = &it.subtitle {
                max_label = max_label.max(
                    self.engine
                        .measure(sub, &TextStyle::new(MENU_FONT - 2.5), None)
                        .w,
                );
            }
            let tw = if !it.submenu.is_empty() {
                10
            } else if let Some(s) = &it.shortcut {
                self.engine
                    .measure(s, &TextStyle::new(MENU_FONT - 2.0), None)
                    .w
            } else if it.checked {
                12
            } else {
                0
            };
            let mut total = tw;
            if let Some((text, _)) = &it.badge {
                let bw = self.engine.measure(text, &TextStyle::new(12.0), None).w + 2 * BADGE_PAD_X;
                total += if total > 0 { MENU_GAP } else { 0 } + bw;
            }
            if it.trailing_icon.is_some() {
                total += if total > 0 { MENU_GAP } else { 0 } + MENU_ICON_W;
            }
            max_trail = max_trail.max(total);
        }
        let icon_w = if has_icons { MENU_ICON_W + MENU_GAP } else { 0 };
        let trail_w = if max_trail > 0 {
            MENU_TRAIL_GAP + max_trail
        } else {
            0
        };
        let w = (MENU_PAD_X + icon_w + max_label + trail_w + MENU_PAD_X)
            .max(MENU_MIN_W)
            .max(min_width);
        (w, has_icons)
    }

    /// 构造一级面板：锚点 (ax, ay) 为期望左上角；越窗右缘时按 `flip_right` 左翻；
    /// 越窗下缘时：若 `anchor_top` 有值（下拉控件顶部 y），优先向上翻转（菜单底对齐控件顶），
    /// 保证控件自身不被遮挡；否则退化为向上钳制。
    pub(super) fn build_level(
        &mut self,
        items: Vec<MenuItem>,
        ax: i32,
        ay: i32,
        min_width: i32,
        flip_right: Option<i32>,
        anchor_top: Option<i32>,
    ) -> MenuLevel {
        // 空组留下的孤立/相邻分隔线在此收口（见 `normalize_separators`）。所有层——根层
        // 与各级子菜单——都经过这里，故规范化对整棵菜单生效。
        let mut items = items;
        normalize_separators(&mut items);
        let (w, has_icons) = self.level_width(&items, min_width);
        let body: i32 = items.iter().map(menu_item_height).sum();
        let content_h = body + 2 * MENU_VPAD;
        // 面板可视高度：不超过 MENU_MAX_H，也不超过窗口高的 3/4。
        let ws = self.logical_size;
        let max_h = MENU_MAX_H.min(if ws.h > 0 { ws.h * 3 / 4 } else { MENU_MAX_H });
        let h = content_h.min(max_h);
        let mut x = ax;
        let mut y = ay;
        // MENU_EDGE_MARGIN：弹层与窗口四边保留距离，避免滚动条落入 resize 边框区。
        let em = if ws.w > 0 { MENU_EDGE_MARGIN } else { 0 };
        if ws.w > 0 && x + w > ws.w - em {
            x = match flip_right {
                Some(parent_left) => (parent_left - w).max(em),
                None => (ws.w - w - em).max(em),
            };
        }
        x = x.max(em);
        if ws.h > 0 && y + h > ws.h - em {
            if let Some(top) = anchor_top {
                // 下拉控件：优先向上翻转（菜单底对齐控件顶），避免遮住控件。
                // 若上方空间也不足，取上下哪侧空间大的一侧并钳制。
                let y_above = top - h;
                if y_above >= em {
                    y = y_above;
                } else {
                    let space_below = ws.h - ay;
                    let space_above = top;
                    if space_above >= space_below {
                        y = em; // 上方更大，贴顶留边
                    } else {
                        y = (ws.h - h - em).max(em); // 下方更大，贴底留边
                    }
                }
            } else {
                y = (ws.h - h - em).max(em);
            }
        }
        y = y.max(em);
        // 计算初始滚动偏移：使 checked 项（当前选中）居中于可视区域。
        let initial_scroll = if content_h > h {
            let mut offset = MENU_VPAD;
            let mut result = 0i32;
            for it in &items {
                let ih = menu_item_height(it);
                if it.checked {
                    result = offset + ih / 2 - h / 2;
                    break;
                }
                offset += ih;
            }
            result.clamp(0, (content_h - h).max(0))
        } else {
            0
        };
        MenuLevel {
            items,
            rect: Rect::new(x, y, w, h),
            hover: None,
            has_icons,
            spawn: None,
            content_h,
            scroll: initial_scroll,
        }
    }

    /// 打开上下文菜单（根级）。
    pub(super) fn open_menu(&mut self, req: crate::event::MenuRequest, target: NodeId) {
        let level = self.build_level(
            req.items,
            req.pos.x,
            req.pos.y,
            req.min_width,
            None,
            req.anchor_top,
        );
        self.menu.active = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: req.rebuild,
        });
    }

    /// 关闭浮层菜单，并标记下一帧整窗重绘。
    ///
    /// 浮层画在控件树之上、不属于任何节点，故不在任何控件的交互脏区内。而 `render` 的
    /// `overlay` 判定问的是"**本帧**有没有浮层"——关闭帧已经没有了，此时若恰好存在一小块
    /// 脏区（如打开菜单时清 hover 触发的边框补间仍在跑），就会走局部重绘，只擦那一小块，
    /// 面板像素留在屏上。关闭浮层必经此处，勿直接写 `self.menu.active = None`。
    pub(super) fn close_menu(&mut self) {
        self.menu.active = None;
        self.damage.needs_full = true;
    }

    /// 清各级悬停高亮。返回是否有变化（有则请求重绘）。
    ///
    /// **不收起已展开的子菜单**：指针滑出面板不等于放弃选择，桌面惯例是保持展开到点别处；
    /// 且展开了子菜单的父项仍由 `child_spawn` 维持高亮（见菜单绘制处的 `active` 判定），
    /// 清 hover 不会让那一项也跟着灭掉。
    fn clear_menu_hover(&mut self) -> bool {
        let Some(m) = self.menu.active.as_mut() else {
            return false;
        };
        let mut changed = false;
        for lvl in m.levels.iter_mut() {
            if lvl.hover.is_some() {
                lvl.hover = None;
                changed = true;
            }
        }
        changed
    }

    /// 按指针位置更新悬停路径：设置所在层悬停项，并按需展开/收起其级联子菜单。
    fn menu_hover_update(&mut self, pos: Point) -> bool {
        let Some(k) = self.menu.active.as_ref().and_then(|m| m.level_at(pos)) else {
            // 指针移到所有面板之外：清掉残留高亮。
            //
            // ★ 此前这里直接 return false——什么都不改，于是最后停留过的那一项一直亮着，
            //   指针早已在菜单外，看着像"这一项被选中了"。控件树里的 hover 有 Enter/Leave
            //   兜底，菜单浮层不走那条路（它有独立命中逻辑），得在这里自己收。
            return self.clear_menu_hover();
        };
        let item_idx = self.menu.active.as_ref().unwrap().levels[k].item_at(pos);
        let mut changed = false;
        {
            let m = self.menu.active.as_mut().unwrap();
            if m.levels[k].hover != item_idx {
                m.levels[k].hover = item_idx;
                changed = true;
            }
        }
        // 悬停项是否有可展开的子菜单（锚点计算与压栈见 menu_spawn_submenu，键盘 → 共用）。
        let spawnable = {
            let lvl = &self.menu.active.as_ref().unwrap().levels[k];
            matches!(item_idx, Some(i) if !lvl.items[i].submenu.is_empty() && lvl.items[i].enabled)
        };
        let existing_spawn = self
            .menu
            .active
            .as_ref()
            .and_then(|m| m.levels.get(k + 1).map(|l| l.spawn));
        match item_idx {
            Some(i) if spawnable => {
                if existing_spawn == Some(Some(i)) {
                    // 该子菜单已展开：仅收起更深层。
                    let m = self.menu.active.as_mut().unwrap();
                    if m.levels.len() > k + 2 {
                        m.levels.truncate(k + 2);
                        changed = true;
                    }
                } else {
                    changed |= self.menu_spawn_submenu(k, i);
                }
            }
            _ => {
                // 悬停项无子菜单：收起本层之下的所有子菜单。
                let m = self.menu.active.as_mut().unwrap();
                if m.levels.len() > k + 1 {
                    m.levels.truncate(k + 1);
                    changed = true;
                }
            }
        }
        changed
    }

    /// 菜单激活时处理指针；返回是否需重绘。
    pub(super) fn handle_menu_pointer(&mut self, ev: PointerEvent) -> bool {
        match ev.kind {
            PointerKind::Move => {
                // 滚动条拖拽中：按拖拽量更新 scroll，不做悬停高亮。
                if let Some(drag) = &self.menu.scrollbar_drag {
                    let dy = ev.pos.y - drag.start_y;
                    let travel = (drag.track_h - drag.thumb_h).max(1.0);
                    let level_idx = drag.level;
                    let start_scroll = drag.start_scroll;
                    if let Some(m) = self.menu.active.as_mut() {
                        if let Some(level) = m.levels.get_mut(level_idx) {
                            let max_sc = level.max_scroll();
                            let new_scroll =
                                start_scroll + (dy as f32 * max_sc as f32 / travel).round() as i32;
                            level.scroll = new_scroll.clamp(0, max_sc);
                        }
                    }
                    return true;
                }
                self.menu_hover_update(ev.pos)
            }
            PointerKind::Down => {
                // 滚动条命中检测：面板右侧 10px 区域内且该面板有滚动内容。
                if let Some(k) = self.menu.active.as_ref().and_then(|m| m.level_at(ev.pos)) {
                    let level = &self.menu.active.as_ref().unwrap().levels[k];
                    // 命中滚动条：开始拖拽，不关闭菜单也不触发项。几何取自
                    // scrollbar_geom，与绘制同源——纵向也必须落在轨道内。
                    if let Some((track, _, thumb_h)) = level
                        .scrollbar_geom()
                        .filter(|(t, _, _)| t.contains(ev.pos))
                    {
                        self.menu.scrollbar_drag = Some(MenuScrollbarDrag {
                            level: k,
                            start_y: ev.pos.y,
                            start_scroll: level.scroll,
                            track_h: track.h as f32,
                            thumb_h,
                        });
                        self.swallow_up = true;
                        return true;
                    }
                }
                // 常规 Down：关闭菜单（命中叶子项执行后关 / 点外关）。
                self.swallow_up = true;
                let Some(k) = self.menu.active.as_ref().and_then(|m| m.level_at(ev.pos)) else {
                    self.close_menu(); // 点击所有面板之外：关闭
                    return true;
                };
                // 同步悬停路径（保证子菜单按当前指针展开）。
                self.menu_hover_update(ev.pos);
                // 尾随可点击图标优先命中：独立于主项 action，点击只触发图标自己的回调
                // （如"删除该项"），不触发本项被选中。
                let trailing_hit = self
                    .menu
                    .active
                    .as_ref()
                    .and_then(|m| m.levels[k].trailing_icon_at(ev.pos))
                    .and_then(|i| {
                        self.menu.active.as_ref().unwrap().levels[k].items[i]
                            .on_trailing_click
                            .clone()
                    });
                if let Some(f) = trailing_hit {
                    let target = self.menu.active.as_ref().map(|m| m.target);
                    self.close_menu();
                    self.run_menu_action(target, |ctx| f(ctx));
                    return true;
                }
                // 命中项：叶子执行并关闭；子菜单父项/禁用项保持展开。
                let hit = self.menu.active.as_ref().and_then(|m| {
                    let lvl = &m.levels[k];
                    lvl.item_at(ev.pos).map(|i| lvl.items[i].clone())
                });
                if let Some(item) = hit {
                    return self.activate_menu_item(item);
                }
                true
            }
            PointerKind::Up => {
                // 结束滚动条拖拽（若有）。
                self.menu.scrollbar_drag = None;
                true
            }
            PointerKind::Wheel(delta) => {
                // 滚轮在菜单面板内滚动：delta>0=上滚（内容下移，scroll 减小）。
                if let Some(k) = self.menu.active.as_ref().and_then(|m| m.level_at(ev.pos)) {
                    let level = &mut self.menu.active.as_mut().unwrap().levels[k];
                    let step = (delta.abs() / 3).max(MENU_ITEM_H);
                    let dir = if delta > 0 { -step } else { step };
                    level.scroll = (level.scroll + dir).clamp(0, level.max_scroll());
                }
                true
            }
            _ => true, // 其余事件吞掉，避免穿透到下层
        }
    }

    /// 执行一个菜单项：粘滞项原地刷新勾选态、菜单留在原处，其余关闭菜单后执行。
    /// 非 actionable（分隔线 / 禁用 / 子菜单父项）为空操作。
    ///
    /// 指针命中与键盘回车共用此处——两条入口各写一份迟早会分叉（粘滞项、SendKey
    /// 的关闭时机都藏在这里）。
    fn activate_menu_item(&mut self, item: MenuItem) -> bool {
        if !item.is_actionable() {
            return true;
        }
        let target = self.menu.active.as_ref().map(|m| m.target);
        // 粘滞项（复选菜单的开关）：执行后菜单留在原地并刷新勾选态，可连点多个开关。
        if item.stay_open {
            if let MenuAction::Run(f) = item.action {
                // 先刷新再落副作用：动作若自己又请求了新菜单，`apply_dispatch_effects`
                // 会把浮层整个换掉，此时再按旧重建器刷新就是刷一个已经不在的菜单。
                let res = target.map(|t| self.tree.run_detached(t, |ctx| f(ctx)));
                if let Some(m) = self.menu.active.as_mut() {
                    m.refresh_items();
                }
                if let Some(res) = res {
                    self.apply_dispatch_effects(res, FocusSource::Pointer, None);
                }
            }
            return true;
        }
        self.close_menu();
        match item.action {
            MenuAction::SendKey(key) => {
                if let Some(t) = target {
                    let res = self.tree.dispatch_key(key, Some(t));
                    if res.close {
                        self.apply_close_intent();
                    }
                }
            }
            MenuAction::Run(f) => self.run_menu_action(target, |ctx| f(ctx)),
        }
        true
    }

    /// 执行一个菜单动作闭包：借目标控件的 `EventCtx` 跑它（见 `Tree::run_detached`），
    /// 副作用交给 `apply_dispatch_effects` 落地——与指针/键盘分发同一条消费路径，
    /// 将来给 `DispatchResult` 加字段时不会独独漏掉菜单这一路。
    ///
    /// `repaint`/`damage` 刻意丢弃：菜单路径本就整窗重绘（`close_menu` 与粘滞刷新
    /// 都置 `needs_full`），再合并一次局部脏区没有意义。
    ///
    /// `target` 是弹出菜单时记下的控件（`ContextMenu::target`），已随浮层关闭取出；
    /// 它可能已失效，`run_detached` 对死节点是安全的。
    fn run_menu_action(
        &mut self,
        target: Option<NodeId>,
        f: impl FnOnce(&mut crate::core::EventCtx),
    ) {
        let Some(t) = target.or(self.tree.root) else {
            return;
        };
        let res = self.tree.run_detached(t, f);
        self.apply_dispatch_effects(res, FocusSource::Pointer, None);
    }

    /// 在第 `k` 级的第 `i` 项上展开子菜单：截断更深层后压入新级。返回是否压入。
    /// 锚点为父项右缘、顶部对齐该项；鼠标悬停展开与键盘 → 共用此处。
    fn menu_spawn_submenu(&mut self, k: usize, i: usize) -> bool {
        let Some(m) = self.menu.active.as_ref() else {
            return false;
        };
        let Some(lvl) = m.levels.get(k) else {
            return false;
        };
        let Some(it) = lvl.items.get(i) else {
            return false;
        };
        if it.submenu.is_empty() || !it.enabled {
            return false;
        }
        let items = it.submenu.clone();
        let (top, _) = lvl.item_rows()[i];
        let (ax, ay, parent_left) = (lvl.rect.right(), top - MENU_VPAD, lvl.rect.x);
        if let Some(m) = self.menu.active.as_mut() {
            m.levels.truncate(k + 1);
        }
        let mut child = self.build_level(items, ax - 2, ay, 0, Some(parent_left + 2), None);
        child.spawn = Some(i);
        self.menu.active.as_mut().unwrap().levels.push(child);
        true
    }

    /// 最深一级面板的下标（菜单必然非空时才调用）。
    fn menu_top_level(&self) -> Option<usize> {
        self.menu
            .active
            .as_ref()
            .and_then(|m| m.levels.len().checked_sub(1))
    }

    /// 设置第 `k` 级高亮项：收起其下已展开的子菜单（同鼠标移开），并滚进可视区。
    /// 有子菜单的项不在此自动展开——键盘上由 → 显式进入（同 Windows 菜单）。
    fn menu_set_hover(&mut self, k: usize, i: usize) {
        if let Some(m) = self.menu.active.as_mut() {
            if m.levels.len() > k + 1 {
                m.levels.truncate(k + 1);
            }
            if let Some(lvl) = m.levels.get_mut(k) {
                lvl.hover = Some(i);
            }
        }
        self.menu_scroll_into_view(k, i);
    }

    /// 把第 `k` 级的第 `i` 项滚进面板可视区（已在视口内则不动）。
    /// 内容坐标 `off` 与 `MenuLevel::item_rows` 同源：`MENU_VPAD` + 前序项高之和，
    /// 屏幕 y = `rect.y + off - scroll`，故可视范围即 `[scroll, scroll + rect.h]`。
    fn menu_scroll_into_view(&mut self, k: usize, i: usize) {
        let Some(m) = self.menu.active.as_mut() else {
            return;
        };
        let Some(lvl) = m.levels.get_mut(k) else {
            return;
        };
        let Some(h) = lvl.items.get(i).map(menu_item_height) else {
            return;
        };
        let off = MENU_VPAD + lvl.items.iter().take(i).map(menu_item_height).sum::<i32>();
        let max_sc = lvl.max_scroll();
        if off < lvl.scroll {
            lvl.scroll = off;
        } else if off + h > lvl.scroll + lvl.rect.h {
            lvl.scroll = off + h - lvl.rect.h;
        }
        lvl.scroll = lvl.scroll.clamp(0, max_sc);
    }

    /// ↑/↓：最深层内移动高亮，跳过分隔线与禁用项，到头循环。
    ///
    /// 尚无高亮时落到 checked 项（下拉的当前选中），没有则落到首/末项——让键盘用户
    /// 先看清起点在哪，而不是凭空跳走一格。
    fn menu_move_hover(&mut self, forward: bool) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let lvl = &self.menu.active.as_ref().unwrap().levels[k];
        let sel: Vec<usize> = (0..lvl.items.len())
            .filter(|&i| menu_item_selectable(&lvl.items[i]))
            .collect();
        if sel.is_empty() {
            return true;
        }
        let target = match lvl.hover.and_then(|h| sel.iter().position(|&i| i == h)) {
            Some(p) => {
                let step = if forward { 1 } else { sel.len() - 1 };
                sel[(p + step) % sel.len()]
            }
            None => sel
                .iter()
                .copied()
                .find(|&i| lvl.items[i].checked)
                .unwrap_or(if forward { sel[0] } else { sel[sel.len() - 1] }),
        };
        self.menu_set_hover(k, target);
        true
    }

    /// Home/End：跳到本级首个/末个可选项。
    fn menu_jump_hover(&mut self, first: bool) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let lvl = &self.menu.active.as_ref().unwrap().levels[k];
        let target = if first {
            lvl.items.iter().position(menu_item_selectable)
        } else {
            lvl.items.iter().rposition(menu_item_selectable)
        };
        if let Some(i) = target {
            self.menu_set_hover(k, i);
        }
        true
    }

    /// →：进入当前高亮项的子菜单，高亮落到子菜单首个可选项。无子菜单则不动。
    fn menu_enter_submenu(&mut self) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let Some(i) = self.menu.active.as_ref().unwrap().levels[k].hover else {
            return true;
        };
        if !self.menu_spawn_submenu(k, i) {
            return true;
        }
        let nk = self.menu.active.as_ref().unwrap().levels.len() - 1;
        let first = self.menu.active.as_ref().unwrap().levels[nk]
            .items
            .iter()
            .position(menu_item_selectable);
        if let Some(f) = first {
            self.menu_set_hover(nk, f);
        }
        true
    }

    /// ←：收起最深一级回到父级。已在根级则不动（不关闭整个菜单，同 Windows 菜单）。
    fn menu_leave_level(&mut self) -> bool {
        if let Some(m) = self.menu.active.as_mut() {
            if m.levels.len() > 1 {
                m.levels.pop();
            }
        }
        true
    }

    /// Enter/Space：激活当前高亮项。子菜单父项等同 →（展开而非执行）。
    fn menu_activate_hover(&mut self) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let m = self.menu.active.as_ref().unwrap();
        let Some(i) = m.levels[k].hover else {
            return true;
        };
        let Some(item) = m.levels[k].items.get(i).cloned() else {
            return true;
        };
        if !item.submenu.is_empty() {
            return self.menu_enter_submenu();
        }
        self.activate_menu_item(item)
    }

    /// 菜单激活时处理键盘；返回是否需重绘。
    ///
    /// 键盘与指针是菜单的两套并行入口：指针按坐标命中（`handle_menu_pointer`），
    /// 键盘按最深层的 hover 下标走，两者共用 `activate_menu_item` /
    /// `menu_spawn_submenu`。未识别的键一律吞掉——菜单是模态浮层，放行会让按键
    /// 打到被遮住的控件上。
    pub(super) fn handle_menu_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        if !ev.pressed {
            return true;
        }
        match ev.key {
            Key::Escape => {
                self.close_menu();
                true
            }
            Key::Down => self.menu_move_hover(true),
            Key::Up => self.menu_move_hover(false),
            Key::Home => self.menu_jump_hover(true),
            Key::End => self.menu_jump_hover(false),
            Key::Right => self.menu_enter_submenu(),
            Key::Left => self.menu_leave_level(),
            Key::Enter | Key::Space => self.menu_activate_hover(),
            // Tab 不在菜单里导航焦点：先收起浮层，让焦点回到发起控件。
            Key::Tab => {
                self.close_menu();
                true
            }
            _ => true,
        }
    }
}

impl MenuHost {
    /// 绘制级联菜单浮层：从根到子菜单逐级绘制（子菜单覆盖在上）。
    /// 由 `render` 在控件树与 toast 之后调用，确保菜单不被 toast 遮挡。
    pub(super) fn paint(&self, canvas: &mut dyn Canvas, theme: &Theme) {
        let Some(menu) = self.active.as_ref() else {
            return;
        };
        let (pal, mt) = (&theme.palette, &theme.menu);
        for (li, level) in menu.levels.iter().enumerate() {
            let r = level.rect;
            // 面板投影 + 圆角底 + 描边。投影参数走主题（见 `MenuTheme::shadow`）。
            let sh = mt.shadow();
            canvas.draw_shadow(
                r.x as f32 + sh.dx,
                r.y as f32 + sh.dy,
                r.w as f32,
                r.h as f32,
                10.0,
                sh.blur,
                sh.color,
            );
            canvas.fill_round_rect(
                r.x as f32,
                r.y as f32,
                r.w as f32,
                r.h as f32,
                10.0,
                &Paint::fill(mt.bg(pal)),
            );
            canvas.stroke_round_rect(
                r.x as f32,
                r.y as f32,
                r.w as f32,
                r.h as f32,
                10.0,
                1.0,
                &Paint::fill(mt.border(pal)),
            );
            let child_spawn = menu.levels.get(li + 1).and_then(|l| l.spawn);
            let label_x = r.x
                + MENU_PAD_X
                + if level.has_icons {
                    MENU_ICON_W + MENU_GAP
                } else {
                    0
                };
            // 裁剪到内缩矩形（`item_clip` 同时是命中判据，见其文档）：上下各留
            // MENU_VPAD 像素，使条目在触达圆角边框前自然裁切（scroll=0 时第一项
            // 恰在裁剪边界，滚动时产生平滑"滚出"效果）。
            canvas.save();
            canvas.clip_rect(level.item_clip());
            for (i, (top, h)) in level.item_rows().into_iter().enumerate() {
                let it = &level.items[i];
                if it.separator {
                    canvas.fill_rect(
                        (r.x + 8) as f32,
                        (top + h / 2) as f32,
                        (r.w - 16) as f32,
                        1.0,
                        &Paint::fill(mt.border(pal)),
                    );
                    continue;
                }
                // 激活：本层悬停项，或展开了子菜单的父项（指针深入子菜单时父项保持高亮）。
                let active = (level.hover == Some(i) || child_spawn == Some(i)) && it.enabled;
                if active {
                    canvas.fill_round_rect(
                        (r.x + 4) as f32,
                        (top + 1) as f32,
                        (r.w - 8) as f32,
                        (h - 2) as f32,
                        6.0,
                        &Paint::fill(mt.hover(pal)),
                    );
                }
                // 取色优先级：禁用 > intent > 悬停/勾选 > 常规。
                // intent 压过悬停是有意的——危险项被指向时更该保持红，而不是变成与
                // 「复制」同款的强调色；禁用压过 intent 也是——不可点的项不该还在喊危险。
                let color = match (it.enabled, it.intent) {
                    (false, _) => mt.text_disabled(pal),
                    (true, Some(intent)) => intent.badge_colors(pal).1,
                    _ if active || it.checked => mt.accent(pal),
                    _ => mt.text(pal),
                };
                // 图标列。
                if let Some(icon) = &it.icon {
                    let ir = Rect::new(r.x + MENU_PAD_X, top, MENU_ICON_W, h);
                    canvas.draw_text(
                        icon,
                        ir,
                        color,
                        crate::spec::Align::Center,
                        &TextStyle::new(MENU_FONT),
                    );
                }
                // 尾随区域从右向左依次收窄：可点击图标 → 徽章胶囊 → 剩余内容右边界。
                let mut content_right = r.right() - MENU_PAD_X;
                if let Some(icon) = &it.trailing_icon {
                    let ir = Rect::new(content_right - MENU_ICON_W, top, MENU_ICON_W, h);
                    canvas.draw_text(
                        icon,
                        ir,
                        color,
                        crate::spec::Align::Center,
                        &TextStyle::new(MENU_FONT),
                    );
                    content_right -= MENU_ICON_W + MENU_GAP;
                }
                if let Some((text, intent)) = &it.badge {
                    let (fill, fg) = intent.badge_colors(pal);
                    let bw = canvas.measure_text(text, &TextStyle::new(12.0)).w + 2 * BADGE_PAD_X;
                    let br = Rect::new(content_right - bw, top + (h - BADGE_H) / 2, bw, BADGE_H);
                    canvas.fill_round_rect(
                        br.x as f32,
                        br.y as f32,
                        br.w as f32,
                        br.h as f32,
                        999.0,
                        &Paint::fill(fill),
                    );
                    canvas.draw_text(
                        text,
                        br,
                        fg,
                        crate::spec::Align::Center,
                        &TextStyle::new(12.0),
                    );
                    content_right -= bw + MENU_GAP;
                }
                // 标签（+ 可选第二行小字说明）。
                let label_w = (content_right - label_x).max(0);
                if let Some(sub) = &it.subtitle {
                    let lr = Rect::new(label_x, top, label_w, h / 2);
                    canvas.draw_text(
                        &it.label,
                        lr,
                        color,
                        crate::spec::Align::Start,
                        &TextStyle::new(MENU_FONT),
                    );
                    let sr = Rect::new(label_x, top + h / 2, label_w, h - h / 2);
                    canvas.draw_text(
                        sub,
                        sr,
                        mt.text_disabled(pal),
                        crate::spec::Align::Start,
                        &TextStyle::new(MENU_FONT - 2.5),
                    );
                } else {
                    let lr = Rect::new(label_x, top, label_w, h);
                    canvas.draw_text(
                        &it.label,
                        lr,
                        color,
                        crate::spec::Align::Start,
                        &TextStyle::new(MENU_FONT),
                    );
                }
                // 尾随：子菜单箭头 › / 快捷键 / 勾选（收窄到 content_right，避免与徽章/图标重叠）。
                let tr = Rect::new(r.x, top, (content_right - r.x).max(0), h);
                if !it.submenu.is_empty() {
                    canvas.draw_text(
                        "\u{203A}",
                        tr,
                        color,
                        crate::spec::Align::End,
                        &TextStyle::new(MENU_FONT + 1.0),
                    );
                } else if let Some(s) = &it.shortcut {
                    canvas.draw_text(
                        s,
                        tr,
                        mt.text_disabled(pal),
                        crate::spec::Align::End,
                        &TextStyle::new(MENU_FONT - 2.0),
                    );
                } else if it.checked {
                    canvas.draw_text(
                        "\u{2713}",
                        tr,
                        mt.accent(pal),
                        crate::spec::Align::End,
                        &TextStyle::new(MENU_FONT),
                    );
                }
            }
            canvas.restore();
            // 内容超高时绘制右侧滚动指示条（几何与命中同源，见 scrollbar_geom）。
            if let Some((_, thumb_y, thumb_h)) = level.scrollbar_geom() {
                canvas.fill_round_rect(
                    (r.right() - 8) as f32,
                    thumb_y,
                    5.0,
                    thumb_h,
                    2.5,
                    &Paint::fill(mt.border(pal)),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{dropdown_handler, key_ev};
    use crate::app::App;
    use crate::ui::Element;

    /// 回归：滚动条的命中区纵向必须与轨道一致。此前判据只写了 `x >= right - 16`，
    /// 完全不约束 y，而滑块只画在 `y+4` 起、高 `h-8` 的轨道内——点面板右缘最顶或
    /// 最底那几像素，会开始拖一个视觉上不在那里的滑块。横向仍留 16px 容错（可见
    /// 滑块只有 5px 宽，不留容错就太难点中），纵向不留。
    #[test]
    fn scrollbar_hit_area_matches_the_drawn_track_vertically() {
        use crate::event::MenuItem;
        use crate::geometry::Point;

        let items: Vec<MenuItem> = (0..10)
            .map(|i| MenuItem::run(format!("项 {i}"), |_ctx| {}, false))
            .collect();
        let level = MenuLevel {
            items,
            rect: Rect::new(40, 100, 160, 5 * MENU_ITEM_H + 2 * MENU_VPAD),
            hover: None,
            has_icons: false,
            spawn: None,
            content_h: 10 * MENU_ITEM_H + 2 * MENU_VPAD,
            scroll: 0,
        };
        let (track, _, _) = level.scrollbar_geom().expect("内容超高应有滚动条");
        let r = level.rect;
        let x = r.right() - 3; // 落在 16px 命中区内

        // 前置：这两个点确实在面板内、且横向落在旧判据 (x >= right-16) 的范围里，
        // 否则本测试不修也会绿。
        for y in [r.y + 1, r.bottom() - 2] {
            assert!(r.contains(Point::new(x, y)), "前置：点应在面板矩形内");
            assert!(x >= r.right() - SCROLLBAR_HIT_W, "前置：横向应落在命中区");
            assert!(
                !track.contains(Point::new(x, y)),
                "y={y} 在轨道之外（轨道 {}..{}），不得判为命中滚动条",
                track.y,
                track.bottom()
            );
        }
        // 轨道内照常命中，别把正常拖拽一起关掉。
        assert!(track.contains(Point::new(x, track.y + track.h / 2)));
    }

    /// 回归：命中判据必须与绘制裁剪同源。面板上下各 `MENU_VPAD` 的边带被 clip 掉，
    /// 而 `scroll == 0` 时首项恰好从裁剪线起画，边带里没有行——问题只在滚动之后
    /// 暴露：挪进边带的行看不见却仍落在 `rect` 内，按 `rect.contains` 判定就是
    /// 命中，用户点到的是一个屏幕上不存在的项。
    #[test]
    fn scrolled_menu_ignores_clicks_in_clipped_bands() {
        use crate::event::MenuItem;
        use crate::geometry::Point;

        let items: Vec<MenuItem> = (0..10)
            .map(|i| MenuItem::run(format!("项 {i}"), |_ctx| {}, false))
            .collect();
        let level = MenuLevel {
            items,
            // 面板只装得下 5 行，内容有 10 行 → 可滚动。
            rect: Rect::new(40, 100, 160, 5 * MENU_ITEM_H + 2 * MENU_VPAD),
            hover: None,
            has_icons: false,
            spawn: None,
            content_h: 10 * MENU_ITEM_H + 2 * MENU_VPAD,
            // 滚半行：行边界落在边带内部，两条边带底下都确实压着行。
            scroll: MENU_ITEM_H / 2,
        };
        let (r, x) = (level.rect, level.rect.x + 20);
        let rows = level.item_rows();
        let covered = |y: i32| rows.iter().any(|(top, h)| y >= *top && y < top + h);

        for y in [r.y + MENU_VPAD - 1, r.bottom() - MENU_VPAD] {
            let p = Point::new(x, y);
            // 前置条件：该点在面板内（不是"点到菜单外"那条已有的关闭路径），
            // 且确实被某一行覆盖——否则这个断言不修也会通过。
            assert!(r.contains(p), "y={y} 应落在面板矩形内");
            assert!(covered(y), "y={y} 应被某一行覆盖（否则测不到本缺陷）");
            assert_eq!(
                level.item_at(p),
                None,
                "y={y} 在被裁掉的边带里，不得命中任何项"
            );
            assert_eq!(
                level.trailing_icon_at(p),
                None,
                "y={y} 尾随图标同样不得命中"
            );
        }

        // 可视带内照常命中，修复没有把正常路径一起关掉。
        assert!(
            level.item_at(Point::new(x, r.y + MENU_VPAD)).is_some(),
            "裁剪线上第一行像素应可点"
        );
        assert!(
            level
                .item_at(Point::new(x, r.bottom() - MENU_VPAD - 1))
                .is_some(),
            "裁剪线下最后一行像素应可点"
        );
    }

    #[test]
    fn trailing_icon_click_fires_independently_of_item_selection() {
        // 回归：菜单项的尾随可点击图标（如"删除该项"）点击只应触发它自己的回调，
        // 不应选中该项——验证 trailing_icon_at 命中 + handle_menu_pointer 优先分支。
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let selected = std::rc::Rc::new(std::cell::Cell::new(false));
        let trashed = std::rc::Rc::new(std::cell::Cell::new(false));
        let (sel, trash) = (selected.clone(), trashed.clone());
        let item = MenuItem::run("团队版", move |_ctx| sel.set(true), false)
            .subtitle("多人协作 + 权限管理")
            .badge("New", crate::theme::Intent::Danger)
            .trailing_icon("🗑", move |_ctx| trash.set(true));

        let level = app.build_level(vec![item], 20, 20, 0, None, None);
        app.menu.active = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: None,
        });

        let rect = app.menu.active.as_ref().unwrap().levels[0].rect;
        let icon_pos = Point::new(
            rect.right() - MENU_PAD_X - MENU_ICON_W / 2,
            rect.y + MENU_VPAD + 5,
        );
        assert_eq!(
            app.menu.active.as_ref().unwrap().levels[0].trailing_icon_at(icon_pos),
            Some(0),
            "尾随图标矩形应命中该项"
        );

        app.handle_menu_pointer(PointerEvent::single(
            PointerKind::Down,
            icon_pos,
            MouseButton::Left,
        ));

        assert!(trashed.get(), "点击尾随图标应触发其自身回调");
        assert!(!selected.get(), "点击尾随图标不应触发主项 action（选中）");
        assert!(app.menu.active.is_none(), "点击后菜单应关闭");
    }

    #[test]
    fn sticky_item_keeps_menu_open_and_refreshes_checks() {
        // 复选菜单的核心回归：开关项点击后菜单**不关闭**、勾选态原地刷新，
        // 可连点多个；混排的动作项仍是"点了执行并关闭"。
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let a = crate::signal::signal(false);
        let b = crate::signal::signal(false);
        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let ran_cb = ran.clone();
        let rebuild: Rc<dyn Fn() -> Vec<MenuItem>> = Rc::new(move || {
            let r = ran_cb.clone();
            vec![
                MenuItem::run("甲", move |_ctx| a.set(!a.get()), a.get()).stay_open(),
                MenuItem::run("乙", move |_ctx| b.set(!b.get()), b.get()).stay_open(),
                MenuItem::run("执行", move |_ctx| r.set(true), false),
            ]
        });

        let level = app.build_level(rebuild(), 20, 20, 0, None, None);
        let rect = level.rect;
        app.menu.active = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: Some(rebuild),
        });
        macro_rules! click {
            ($i:expr) => {
                app.handle_menu_pointer(PointerEvent::single(
                    PointerKind::Down,
                    Point::new(
                        rect.x + 20,
                        rect.y + MENU_VPAD + $i * MENU_ITEM_H + MENU_ITEM_H / 2,
                    ),
                    MouseButton::Left,
                ))
            };
        }

        click!(0);
        assert!(a.get(), "开关项应翻转绑定值");
        assert!(app.menu.active.is_some(), "开关项点击后菜单须保持展开");
        assert!(
            app.menu.active.as_ref().unwrap().levels[0].items[0].checked,
            "重建后勾选态应原地刷新"
        );

        // 连点第二个开关：无需重新打开菜单。
        click!(1);
        assert!(b.get());
        assert!(app.menu.active.is_some());
        let items = &app.menu.active.as_ref().unwrap().levels[0].items;
        assert!(items[0].checked && items[1].checked, "两个开关都应为开");

        // 再点第一个：翻回关闭态，菜单仍在。
        click!(0);
        assert!(!a.get());
        assert!(!app.menu.active.as_ref().unwrap().levels[0].items[0].checked);

        // 混排的动作项不粘滞：执行并关闭。
        click!(2);
        assert!(ran.get(), "动作项应执行");
        assert!(app.menu.active.is_none(), "动作项点击后菜单须关闭");
    }

    /// 回调签名立法的核心回归：菜单项动作拿得到真正的 `EventCtx`，且它请求的副作用
    /// 走的是与控件回调同一条宿主消费路径（toast 上屏、`defer_blocking` 经
    /// `pending_dialog` 出口交给平台）。这正是过去必须绕道自由函数
    /// `app::defer_blocking` 的那个缺口。
    #[test]
    fn menu_action_gets_ctx_and_its_requests_reach_the_host() {
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let ran_cb = ran.clone();
        let item = MenuItem::run(
            "导出…",
            move |ctx| {
                let r = ran_cb.clone();
                ctx.toast("开始导出");
                ctx.defer_blocking(move || r.set(true));
            },
            false,
        );

        let level = app.build_level(vec![item], 20, 20, 0, None, None);
        let rect = level.rect;
        app.menu.active = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: None,
        });
        app.handle_menu_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(rect.x + 20, rect.y + MENU_VPAD + MENU_ITEM_H / 2),
            MouseButton::Left,
        ));

        assert!(app.menu.active.is_none(), "动作项点击后菜单须关闭");
        assert_eq!(app.toast.items.len(), 1, "ctx.toast 应经宿主上屏");
        assert!(!ran.get(), "延迟闭包在取走前不得执行");
        let req = app
            .take_dialog_request()
            .expect("ctx.defer_blocking 应产出对话框请求");
        req.run();
        assert!(ran.get(), "平台执行请求后闭包才跑");
    }

    #[test]
    fn sticky_refresh_preserves_panel_geometry() {
        // 面板宽度/位置不随重建变化：项文本变宽也不重新测量，否则指针下的项会在
        // 两次点击之间挪位，用户点到的不是他瞄准的那一项。
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let wide = crate::signal::signal(false);
        let rebuild: Rc<dyn Fn() -> Vec<MenuItem>> = Rc::new(move || {
            let label = if wide.get() {
                "开关项——展开后标签显著变长以撑宽面板"
            } else {
                "短"
            };
            vec![MenuItem::run(label, move |_ctx| wide.set(!wide.get()), wide.get()).stay_open()]
        });

        let level = app.build_level(rebuild(), 20, 20, 0, None, None);
        let before = level.rect;
        app.menu.active = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: Some(rebuild),
        });
        app.handle_menu_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(before.x + 20, before.y + MENU_VPAD + MENU_ITEM_H / 2),
            MouseButton::Left,
        ));
        assert!(wide.get());
        assert_eq!(
            app.menu.active.as_ref().unwrap().levels[0].rect,
            before,
            "重建不得改变面板矩形"
        );
    }

    /// 条件生成的菜单里，空组留下的分隔线要被收掉：首尾的去掉、相邻的折叠成一条。
    ///
    /// 调用方按 `if 有这项 { push }` 生成、分组线无条件写下，是菜单最自然的写法；
    /// 整组为空时那条线就孤在那儿。让每个调用方自己回溯上一项，必然有漏网的分支。
    #[test]
    fn separators_collapse_around_empty_groups() {
        let item = |s: &str| MenuItem::run(s, |_ctx| {}, false);
        let labels = |v: &Vec<MenuItem>| -> Vec<String> {
            v.iter()
                .map(|i| {
                    if i.separator {
                        "—".into()
                    } else {
                        i.label.clone()
                    }
                })
                .collect()
        };

        // 中间那组（编辑）整组为空 → 两条线挨在一起，应折叠为一条。
        let mut v = vec![
            item("标题"),
            MenuItem::separator(),
            MenuItem::separator(),
            item("复制"),
        ];
        normalize_separators(&mut v);
        assert_eq!(labels(&v), ["标题", "—", "复制"]);

        // 首尾悬空的线。
        let mut v = vec![
            MenuItem::separator(),
            item("只有一项"),
            MenuItem::separator(),
        ];
        normalize_separators(&mut v);
        assert_eq!(labels(&v), ["只有一项"]);

        // 三条连排（连着两组都空）也只留一条。
        let mut v = vec![
            item("A"),
            MenuItem::separator(),
            MenuItem::separator(),
            MenuItem::separator(),
            item("B"),
        ];
        normalize_separators(&mut v);
        assert_eq!(labels(&v), ["A", "—", "B"]);

        // 正常菜单不受影响（幂等）。
        let mut v = vec![item("A"), MenuItem::separator(), item("B")];
        normalize_separators(&mut v);
        let once = labels(&v);
        normalize_separators(&mut v);
        assert_eq!(labels(&v), once);
    }

    /// 刷新要规范化**每一级**，不能只管根层。
    ///
    /// `submenu` 字段里存的始终是原始列表——`build_level` 只规范化它拿到的那一级，
    /// 不会回头去改各项的 `submenu`。所以刷新时若只规范化根层，子级会拿到带孤立分隔线的
    /// 原始列表：本该修掉的空组线在每次粘滞项刷新后复现，而且 `spawn` 记的是规范化后的
    /// 下标，按同一下标去原始列表里取会错位——取到分隔线（`submenu` 为空）就走截断分支，
    /// 把已经展开的孙级菜单静默关掉。
    #[test]
    fn refresh_normalizes_every_level_not_just_the_root() {
        // 用不捕获环境的 fn：`rebuild` 要求 'static，捕获局部闭包会借用超期。
        fn item(s: &str) -> MenuItem {
            MenuItem::run(s, |_ctx| {}, false)
        }
        let labels = |v: &[MenuItem]| -> Vec<String> {
            v.iter()
                .map(|i| {
                    if i.separator {
                        "—".into()
                    } else {
                        i.label.clone()
                    }
                })
                .collect()
        };
        // 子菜单首项是空组遗留的孤立分隔线——正是规范化该收掉的东西。
        fn build() -> Vec<MenuItem> {
            vec![
                item("X"),
                MenuItem::submenu(
                    "更多",
                    vec![
                        MenuItem::separator(),
                        item("G1"),
                        MenuItem::submenu("深", vec![item("D1")]),
                    ],
                ),
            ]
        }
        let host = App::new("t", 60, 60)
            .content(Element::col())
            .into_handler_for_test();
        let target = host.tree.root.expect("根节点");
        // 模拟三级已展开：根 → "更多" → "深"。父级下标取自**规范化后**的列表：
        // 规范化后子菜单是 ["G1", "深"]，故"深"在下标 1。
        let lvl = |items: Vec<MenuItem>, spawn: Option<usize>| MenuLevel {
            items,
            rect: Rect::new(0, 0, 100, 100),
            hover: None,
            has_icons: false,
            spawn,
            content_h: 0,
            scroll: 0,
        };
        let mut sub = build()[1].submenu.clone();
        normalize_separators(&mut sub);
        let mut menu = ContextMenu {
            levels: vec![
                lvl(build(), None),
                lvl(sub, Some(1)),
                lvl(vec![item("D1")], Some(1)),
            ],
            target,
            rebuild: Some(Rc::new(build)),
        };

        menu.refresh_items();

        assert_eq!(menu.levels.len(), 3, "三级都该活着——孙级不能被静默关掉");
        assert_eq!(
            labels(&menu.levels[1].items),
            ["G1", "深"],
            "子级的孤立分隔线该在刷新后依然是收掉的状态"
        );
        assert_eq!(
            labels(&menu.levels[2].items),
            ["D1"],
            "孙级内容应被正确换入"
        );
    }

    /// `danger()` 项用 palette.danger 上色，且**悬停时不退回强调色**；禁用则一律灰。
    #[test]
    fn danger_item_keeps_its_color_over_hover_but_not_over_disabled() {
        let pal = crate::theme::Palette::default();
        // 复刻绘制侧的取色规则（见菜单绘制处的 match）。
        let pick = |it: &MenuItem, active: bool| match (it.enabled, it.intent) {
            (false, _) => pal.text_disabled,
            (true, Some(i)) => i.badge_colors(&pal).1,
            _ if active || it.checked => pal.accent,
            _ => pal.text,
        };
        let del = MenuItem::run("删除", |_ctx| {}, false).danger();
        assert_eq!(pick(&del, false), pal.danger, "常态应为危险色");
        assert_eq!(
            pick(&del, true),
            pal.danger,
            "悬停仍应是危险色，不该变成 accent"
        );
        let del_off = MenuItem::run("删除", |_ctx| {}, false)
            .danger()
            .enabled(false);
        assert_eq!(
            pick(&del_off, false),
            pal.text_disabled,
            "禁用胜过 intent——不可点的项不该还在喊危险"
        );
        let plain = MenuItem::run("复制", |_ctx| {}, false);
        assert_eq!(pick(&plain, false), pal.text);
        assert_eq!(pick(&plain, true), pal.accent, "普通项悬停仍走强调色");
    }

    /// 指针移出菜单面板后不留残影高亮。
    ///
    /// ★ 回归：`menu_hover_update` 在指针落到所有面板之外时直接返回，什么都不改——最后
    /// 停留过的那一项就一直亮着，指针早已在别处，看着像"这一项被选中了"。控件树里的 hover
    /// 有 Enter/Leave 兜底，菜单浮层走独立命中逻辑，不在那条路上，得自己收。
    /// 鼠标移出**整个窗口**时平台层派发的 `Move(-1,-1)`（见 win32 `clear_hover`）同样走这里。
    #[test]
    fn moving_off_the_menu_clears_the_hovered_item() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::{Point, Size};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;

        let app = App::new("t", 200, 200).content(Element::col().width(200).height(200).child(
            Element::dropdown(vec!["甲", "乙", "丙"], crate::signal::signal(0usize)).width(120),
        ));
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));

        let on_ctl = Point::new(40, 12);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            on_ctl,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            on_ctl,
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));
        let panel = handler.menu.active.as_ref().expect("应已展开菜单").levels[0].rect;

        // 停在某一项上 → 该层有 hover。
        let on_item = Point::new(panel.x + panel.w / 2, panel.y + 12);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            on_item,
            MouseButton::Left,
        ));
        assert!(
            handler.menu.active.as_ref().unwrap().levels[0]
                .hover
                .is_some(),
            "指针停在项上应有悬停高亮（否则这条测试没测到东西）"
        );

        // 移到面板外（仍在窗口内）→ 高亮清掉，菜单不关。
        let off = Point::new(panel.right() + 20, panel.y + 12);
        let repaint = handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            off,
            MouseButton::Left,
        ));
        assert!(
            handler.menu.active.as_ref().unwrap().levels[0]
                .hover
                .is_none(),
            "指针移出面板后不该残留高亮"
        );
        assert!(repaint, "高亮变化应请求重绘，否则残影还在屏上");
        assert!(
            handler.menu.active.is_some(),
            "移出面板只清高亮，不该顺手关掉菜单"
        );

        // 鼠标移出整个窗口：平台层发 Move(-1,-1)，同样要清掉。
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            on_item,
            MouseButton::Left,
        ));
        assert!(handler.menu.active.as_ref().unwrap().levels[0]
            .hover
            .is_some());
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            Point::new(-1, -1),
            MouseButton::Left,
        ));
        assert!(
            handler.menu.active.as_ref().unwrap().levels[0]
                .hover
                .is_none(),
            "鼠标离开窗口（Move(-1,-1)）也应清掉菜单高亮"
        );
    }

    /// 回归：Dropdown 一直正确处理 Key::Space（select.rs 的 Key 分支），断的是宿主——
    /// on_key 消费了 close/open_url/window_op/dialog/toast，唯独漏了 res.menu，
    /// 控件的展开请求被 dispatch_key 收进 DispatchResult 后静默丢弃。
    #[test]
    fn keyboard_space_opens_dropdown_menu() {
        use crate::event::KeyEvent;
        use crate::geometry::Size;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let sel = crate::signal::signal(0usize);
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::dropdown(vec!["甲", "乙"], sel).width(200)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));

        let k = |key| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        handler.on_key(k(Key::Tab));
        assert!(handler.focus.current.is_some(), "Tab 应把焦点落到下拉框");
        assert!(handler.menu.active.is_none(), "此时尚无浮层");

        handler.on_key(k(Key::Space));
        assert!(handler.menu.active.is_some(), "按空格应展开下拉菜单");
    }

    /// 菜单展开后的键盘操作：此前 on_key 在 menu.is_some() 时只放行 Escape、
    /// 其余全吞，弹出来也只能用鼠标点。
    #[test]
    fn keyboard_navigates_and_activates_dropdown_menu() {
        use crate::platform::AppHandler;
        let (mut handler, sel) = dropdown_handler();
        let k = key_ev();
        handler.on_key(k(Key::Tab));
        handler.on_key(k(Key::Space));

        // 尚无高亮时首次 ↓ 落到 checked 项（当前选中的第 1 项），而不是凭空跳走一格。
        handler.on_key(k(Key::Down));
        assert_eq!(
            handler.menu.active.as_ref().unwrap().levels[0].hover,
            Some(1),
            "首次 ↓ 应停在当前选中项上"
        );
        handler.on_key(k(Key::Down));
        assert_eq!(
            handler.menu.active.as_ref().unwrap().levels[0].hover,
            Some(2),
            "再次 ↓ 应移到下一项"
        );

        handler.on_key(k(Key::Enter));
        assert!(handler.menu.active.is_none(), "回车执行后菜单应关闭");
        assert_eq!(sel.get(), 2, "回车应选中高亮项");
    }

    #[test]
    fn keyboard_wraps_and_escapes_dropdown_menu() {
        use crate::platform::AppHandler;
        let (mut handler, sel) = dropdown_handler();
        let k = key_ev();
        handler.on_key(k(Key::Tab));
        handler.on_key(k(Key::Space));

        // ↑ 从无高亮起同样落到 checked 项，再 ↑ 回到上一项；首项继续 ↑ 循环到末项。
        handler.on_key(k(Key::Up));
        handler.on_key(k(Key::Up));
        assert_eq!(
            handler.menu.active.as_ref().unwrap().levels[0].hover,
            Some(0)
        );
        handler.on_key(k(Key::Up));
        assert_eq!(
            handler.menu.active.as_ref().unwrap().levels[0].hover,
            Some(2),
            "首项再 ↑ 应循环到末项"
        );

        handler.on_key(k(Key::Escape));
        assert!(handler.menu.active.is_none(), "Escape 应关闭菜单");
        assert_eq!(sel.get(), 1, "Escape 不应改变选中值");
    }

    /// 回归：捕获丢失（Alt+Tab / Cmd+Tab / 原生模态框接管）必须收掉菜单滚动条拖拽。
    ///
    /// 这条拖拽状态不走 `UiHost::capture`——菜单打开时指针事件在进入控件树之前就被
    /// 浮层截走，逻辑捕获从未建立。`on_capture_lost` 里对 `capture` 的判空早退因此
    /// 覆盖不到它：切走应用再回来，滑块还粘在指针上。两平台同病，不是某个后端的事。
    #[test]
    fn capture_loss_aborts_menu_scrollbar_drag() {
        use crate::platform::AppHandler;
        let (mut handler, _sel) = dropdown_handler();
        // 直接置入拖拽态：真实路径要先弹出可滚动的长菜单再按住滑块，
        // 而这里要验的是"捕获丢失时它会不会被收掉"，与怎么进入无关。
        handler.menu.scrollbar_drag = Some(MenuScrollbarDrag {
            level: 0,
            start_y: 50,
            start_scroll: 0,
            track_h: 100.0,
            thumb_h: 20.0,
        });
        assert!(
            handler.capture.is_none(),
            "前置：菜单拖拽期间逻辑捕获本就是空的——正是这一点让早退分支漏掉它"
        );

        let repaint = handler.on_capture_lost();

        assert!(
            handler.menu.scrollbar_drag.is_none(),
            "捕获丢失后拖拽态必须清掉，否则滑块会一直粘着指针"
        );
        assert!(repaint, "收掉拖拽属于可见变化，应请求重绘");
    }
}
