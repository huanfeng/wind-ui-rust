//! 字体仓库：按「族名 + 字重 + 斜体」解析出一条回退链，按字符挑出覆盖它的字体。
//!
//! 进程级共享（一个 `Mutex` 包住）：同一个字体文件不论被几个窗口、几条链引用都只映射
//! 一次。字体文件以 `mmap` 只读映射且**永不卸载**——映射页是文件后备的共享页，不计入
//! 私有内存；动辄 20MB 的 CJK 字体真正触到的只是用过的那些字形所在的页。

use std::collections::HashMap;
use std::ffi::{c_int, c_long, c_void};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::fontconfig;

pub(crate) type FaceId = u32;

/// 一个已加载的字体。度量均为字体单位（`units_per_em` 为分母）。
pub(crate) struct FaceData {
    pub face: ttf_parser::Face<'static>,
    pub upem: f32,
    /// 基线以上高度（正）。
    pub ascent: f32,
    /// 基线以下高度（正）。
    pub descent: f32,
    pub line_gap: f32,
    /// 字体自身的字重（CSS 刻度）与是否斜体——用于判断要不要合成粗体/斜体。
    pub weight: u16,
    pub italic: bool,
}

impl FaceData {
    fn new(face: ttf_parser::Face<'static>) -> Self {
        let upem = face.units_per_em().max(1) as f32;
        // hhea 的 ascender/descender 是桌面上最通行的行距来源；个别字体（部分 CJK）
        // hhea 与 OS/2 typo 差得很远，ttf-parser 已按 USE_TYPO_METRICS 标志择一。
        let mut ascent = face.ascender() as f32;
        let mut descent = -(face.descender() as f32);
        if ascent + descent <= 0.0 {
            ascent = upem * 0.8;
            descent = upem * 0.2;
        }
        Self {
            upem,
            ascent,
            descent,
            line_gap: face.line_gap().max(0) as f32,
            weight: face.weight().to_number(),
            italic: face.is_italic() || face.is_oblique(),
            face,
        }
    }
}

enum Source {
    Fc(fontconfig::Candidate),
    File(PathBuf, u32),
}

struct Cand {
    src: Source,
    /// `None` = 还没加载过；`Some(None)` = 加载失败，不再重试。
    loaded: Option<Option<FaceId>>,
}

impl Cand {
    fn path(&self) -> (&Path, u32) {
        match &self.src {
            Source::Fc(c) => (&c.path, c.index),
            Source::File(p, i) => (p, *i),
        }
    }
    fn may_have(&self, c: char) -> bool {
        match &self.src {
            Source::Fc(fc) => fc.has_char(c),
            Source::File(..) => true,
        }
    }
}

struct Chain {
    cands: Vec<Cand>,
    by_char: HashMap<char, Option<(FaceId, u16)>>,
}

pub(crate) type ChainId = usize;

#[derive(Default)]
pub(crate) struct Store {
    faces: Vec<FaceData>,
    by_path: HashMap<(PathBuf, u32), Option<FaceId>>,
    chains: Vec<Chain>,
    chain_keys: HashMap<(String, u16, bool), ChainId>,
    scanned: Option<Vec<PathBuf>>,
}

pub(crate) fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    static STORE: OnceLock<Mutex<Store>> = OnceLock::new();
    let m = STORE.get_or_init(|| Mutex::new(Store::default()));
    // 持锁期间 panic 只可能来自本模块的逻辑错误；毒化后照用内部数据即可（缓存而已）。
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

/// 默认字族：交给 fontconfig 的通用名，由系统配置决定落到哪款字体。
pub(crate) const DEFAULT_FAMILY: &str = "sans-serif";

impl Store {
    pub fn face(&self, id: FaceId) -> &FaceData {
        &self.faces[id as usize]
    }

    /// 取（或建）一条回退链。
    pub fn chain(&mut self, family: Option<&str>, weight: u16, italic: bool) -> ChainId {
        let fam = family.unwrap_or(DEFAULT_FAMILY);
        let key = (fam.to_string(), weight, italic);
        if let Some(&id) = self.chain_keys.get(&key) {
            return id;
        }
        let cands = match fontconfig::sort(fam, weight, italic, default_lang()) {
            Some(list) if !list.is_empty() => list
                .into_iter()
                .map(|c| Cand {
                    src: Source::Fc(c),
                    loaded: None,
                })
                .collect(),
            _ => {
                static NOTICE: std::sync::Once = std::sync::Once::new();
                NOTICE.call_once(|| {
                    if !fontconfig::available() {
                        log::info!("fontconfig 不可用，改为扫描字体目录选字");
                    }
                });
                self.scan_fallback(fam)
                    .into_iter()
                    .map(|p| Cand {
                        src: Source::File(p, 0),
                        loaded: None,
                    })
                    .collect()
            }
        };
        let id = self.chains.len();
        self.chains.push(Chain {
            cands,
            by_char: HashMap::new(),
        });
        self.chain_keys.insert(key, id);
        id
    }

    /// 链上第一个能加载的字体——行度量（行高、基线）以它为准。
    pub fn primary(&mut self, chain: ChainId) -> Option<FaceId> {
        for i in 0..self.chains[chain].cands.len() {
            if let Some(id) = self.load_cand(chain, i) {
                return Some(id);
            }
        }
        None
    }

    /// 为字符 `c` 挑字体：沿链找第一个 cmap 里有它的。都没有时落回主字体的 `.notdef`
    /// （通常是个方框），好过整字消失、让人以为输入没生效。
    pub fn glyph_for(&mut self, chain: ChainId, c: char) -> Option<(FaceId, u16)> {
        if let Some(hit) = self.chains[chain].by_char.get(&c) {
            return *hit;
        }
        let mut found = None;
        for i in 0..self.chains[chain].cands.len() {
            if !self.chains[chain].cands[i].may_have(c) {
                continue;
            }
            let Some(id) = self.load_cand(chain, i) else {
                continue;
            };
            if let Some(g) = self.faces[id as usize].face.glyph_index(c) {
                if g.0 != 0 {
                    found = Some((id, g.0));
                    break;
                }
            }
        }
        if found.is_none() {
            found = self.primary(chain).map(|id| (id, 0));
        }
        self.chains[chain].by_char.insert(c, found);
        found
    }

    fn load_cand(&mut self, chain: ChainId, i: usize) -> Option<FaceId> {
        if let Some(l) = self.chains[chain].cands[i].loaded {
            return l;
        }
        let (path, index) = self.chains[chain].cands[i].path();
        let key = (path.to_path_buf(), index);
        let id = match self.by_path.get(&key) {
            Some(&r) => r,
            None => {
                let r = map_file(&key.0)
                    .and_then(|data| ttf_parser::Face::parse(data, index).ok())
                    .map(|face| {
                        self.faces.push(FaceData::new(face));
                        (self.faces.len() - 1) as FaceId
                    });
                if r.is_none() {
                    log::warn!("字体加载失败：{}#{index}", key.0.display());
                }
                self.by_path.insert(key, r);
                r
            }
        };
        self.chains[chain].cands[i].loaded = Some(id);
        id
    }

    /// 无 fontconfig 时的兜底：扫常见字体目录，按偏好排序。
    fn scan_fallback(&mut self, family: &str) -> Vec<PathBuf> {
        let all = self.scanned.get_or_insert_with(scan_font_dirs).clone();
        let want = normalize(family);
        let rank = |p: &PathBuf| -> usize {
            let name = normalize(&p.file_name().unwrap_or_default().to_string_lossy());
            if !want.is_empty() && name.contains(&want) {
                return 0;
            }
            PREFERRED
                .iter()
                .position(|k| name.contains(k))
                .map(|i| i + 1)
                .unwrap_or(usize::MAX)
        };
        let mut v: Vec<PathBuf> = all.into_iter().filter(|p| rank(p) != usize::MAX).collect();
        v.sort_by_key(|p| rank(p));
        v
    }
}

/// 无 fontconfig 时的偏好序（小写、去空格与连字符后匹配文件名）。CJK 字体排在前面：
/// 它们同时带拉丁字形，放前面可以让中西文同出一款字体、基线与字重一致。
const PREFERRED: &[&str] = &[
    "notosanscjk",
    "sourcehansans",
    "wqyzenhei",
    "wqymicrohei",
    "droidsansfallback",
    "notosans",
    "dejavusans",
    "liberationsans",
    "ubuntu",
    "cantarell",
];

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn scan_font_dirs() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fonts"),
        PathBuf::from("/usr/local/share/fonts"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push(home.join(".local/share/fonts"));
        roots.push(home.join(".fonts"));
    }
    let mut out = Vec::new();
    let mut stack = roots;
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if matches!(
                p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase())
                    .as_deref(),
                Some("ttf" | "otf" | "ttc" | "otc")
            ) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// 按 locale 给 fontconfig 一个语言提示：同一个汉字在简/繁/日/韩字体里字形不同，
/// 且 `sans-serif` 在 CJK 语言下才会优先落到 CJK 字体上（中西文同源、基线一致）。
pub(crate) fn default_lang() -> Option<&'static str> {
    static LANG: OnceLock<Option<&'static str>> = OnceLock::new();
    *LANG.get_or_init(|| {
        let v = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find(|v| !v.is_empty())?;
        lang_of_locale(&v)
    })
}

fn lang_of_locale(v: &str) -> Option<&'static str> {
    let v = v.to_ascii_lowercase();
    if v.starts_with("zh_tw") || v.starts_with("zh-tw") {
        Some("zh-tw")
    } else if v.starts_with("zh_hk") || v.starts_with("zh-hk") {
        Some("zh-hk")
    } else if v.starts_with("zh") {
        Some("zh-cn")
    } else if v.starts_with("ja") {
        Some("ja")
    } else if v.starts_with("ko") {
        Some("ko")
    } else {
        None
    }
}

extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        off: c_long,
    ) -> *mut c_void;
}

const PROT_READ: c_int = 1;
const MAP_PRIVATE: c_int = 2;

/// 只读映射整个文件，返回永不释放的切片（见模块头：字体不卸载）。
fn map_file(path: &Path) -> Option<&'static [u8]> {
    let f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    // 安全：只读私有映射，长度取自文件元数据；映射在 fd 关闭后依然有效。
    let p = unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_PRIVATE,
            f.as_raw_fd(),
            0,
        )
    };
    if p.is_null() || p as isize == -1 {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(p as *const u8, len) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_maps_to_fontconfig_lang() {
        assert_eq!(lang_of_locale("zh_CN.UTF-8"), Some("zh-cn"));
        assert_eq!(lang_of_locale("zh_TW.UTF-8"), Some("zh-tw"));
        assert_eq!(lang_of_locale("ja_JP.UTF-8"), Some("ja"));
        assert_eq!(lang_of_locale("en_US.UTF-8"), None);
        assert_eq!(lang_of_locale("C"), None);
    }

    #[test]
    fn normalize_strips_separators_and_case() {
        assert_eq!(normalize("Noto Sans-CJK_SC"), "notosanscjksc");
    }
}
