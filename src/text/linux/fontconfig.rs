//! fontconfig 的运行期绑定（`dlopen`）。
//!
//! 为什么不在编译期链接：链接要求构建机装 `libfontconfig1-dev`（`libfontconfig.so` 那个
//! 无版本号的符号链接只在 -dev 包里），下游每台 CI 机都得为此多装一个包；而运行期
//! 桌面系统上 `libfontconfig.so.1` 几乎必然存在。`dlopen` 让「编译」与「有没有
//! fontconfig」脱钩：取不到就返回 `None`，由调用方退回扫字体目录（见 `store.rs`）。
//!
//! 只绑定按名查询 + 按序回退要用的那十来个函数。

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::path::PathBuf;
use std::sync::OnceLock;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 2;
/// `FcResultMatch`。
const FC_RESULT_MATCH: c_int = 0;
/// `FcMatchPattern`。
const FC_MATCH_PATTERN: c_int = 0;

type Ptr = *mut c_void;

/// `FcFontSet` 的内存布局（fontconfig 公开头文件里就是这三个字段，ABI 自 2.0 起未变）。
#[repr(C)]
struct FcFontSet {
    nfont: c_int,
    sfont: c_int,
    fonts: *mut Ptr,
}

struct Api {
    name_parse: unsafe extern "C" fn(*const u8) -> Ptr,
    config_substitute: unsafe extern "C" fn(Ptr, Ptr, c_int) -> c_int,
    default_substitute: unsafe extern "C" fn(Ptr),
    font_sort: unsafe extern "C" fn(Ptr, Ptr, c_int, *mut Ptr, *mut c_int) -> *mut FcFontSet,
    font_set_destroy: unsafe extern "C" fn(*mut FcFontSet),
    pattern_destroy: unsafe extern "C" fn(Ptr),
    pattern_get_string: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut *const u8) -> c_int,
    pattern_get_integer: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut c_int) -> c_int,
    pattern_get_charset: unsafe extern "C" fn(Ptr, *const c_char, c_int, *mut Ptr) -> c_int,
    charset_copy: unsafe extern "C" fn(Ptr) -> Ptr,
    charset_has_char: unsafe extern "C" fn(Ptr, u32) -> c_int,
    charset_destroy: unsafe extern "C" fn(Ptr),
    /// 2.11.91 才有；老版本上缺席时按线性表近似（见 [`fc_weight`]）。
    weight_from_opentype: Option<unsafe extern "C" fn(c_int) -> c_int>,
    config: Ptr,
}

// fontconfig ≥ 2.10 的查询 API 是线程安全的；`config` 在初始化后只读。
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    // 环境变量逃生口：排查「是不是 fontconfig 选错了字」时关掉它，走目录扫描对照。
    if std::env::var_os("WINDUI_NO_FONTCONFIG").is_some() {
        return None;
    }
    unsafe {
        let lib = [c"libfontconfig.so.1", c"libfontconfig.so"]
            .iter()
            .map(|n| dlopen(n.as_ptr(), RTLD_NOW))
            .find(|h| !h.is_null())?;
        macro_rules! sym {
            ($name:literal) => {{
                let p = dlsym(lib, $name.as_ptr());
                if p.is_null() {
                    log::warn!("fontconfig 缺少符号 {:?}，退回目录扫描", $name);
                    return None;
                }
                fn_ptr(p)
            }};
        }
        let init_load_config_and_fonts: unsafe extern "C" fn() -> Ptr =
            sym!(c"FcInitLoadConfigAndFonts");
        let config = init_load_config_and_fonts();
        if config.is_null() {
            return None;
        }
        let wfo = dlsym(lib, c"FcWeightFromOpenType".as_ptr());
        Some(Api {
            name_parse: sym!(c"FcNameParse"),
            config_substitute: sym!(c"FcConfigSubstitute"),
            default_substitute: sym!(c"FcDefaultSubstitute"),
            font_sort: sym!(c"FcFontSort"),
            font_set_destroy: sym!(c"FcFontSetDestroy"),
            pattern_destroy: sym!(c"FcPatternDestroy"),
            pattern_get_string: sym!(c"FcPatternGetString"),
            pattern_get_integer: sym!(c"FcPatternGetInteger"),
            pattern_get_charset: sym!(c"FcPatternGetCharSet"),
            charset_copy: sym!(c"FcCharSetCopy"),
            charset_has_char: sym!(c"FcCharSetHasChar"),
            charset_destroy: sym!(c"FcCharSetDestroy"),
            weight_from_opentype: (!wfo.is_null()).then(|| fn_ptr(wfo)),
            config,
        })
    }
}

/// `dlsym` 取到的地址 → 函数指针。目标类型由调用处的字段类型推出。
///
/// 安全：调用方保证 `p` 非空、且确实是签名为 `F` 的 C 函数（符号名与 fontconfig 公开
/// 头文件的声明一一对应）。`F` 必须是指针大小的函数指针类型。
unsafe fn fn_ptr<F: Copy>(p: *mut c_void) -> F {
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    std::mem::transmute_copy::<*mut c_void, F>(&p)
}

/// fontconfig 是否可用（供诊断日志）。
pub(crate) fn available() -> bool {
    api().is_some()
}

/// 一个候选字体：文件 + 集合内序号 + 覆盖字符集。
pub(crate) struct Candidate {
    pub path: PathBuf,
    pub index: u32,
    charset: Ptr,
}

// charset 是 `FcCharSetCopy` 出来的引用计数副本，只读查询，跨线程安全。
unsafe impl Send for Candidate {}

impl Candidate {
    /// 该字体是否覆盖 `c`。拿不到字符集时按「覆盖」答，交给后面的 cmap 查询兜底。
    pub fn has_char(&self, c: char) -> bool {
        match api() {
            Some(a) if !self.charset.is_null() => unsafe {
                (a.charset_has_char)(self.charset, c as u32) != 0
            },
            _ => true,
        }
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        if let Some(a) = api() {
            if !self.charset.is_null() {
                unsafe { (a.charset_destroy)(self.charset) };
            }
        }
    }
}

/// CSS 字重（100..900）→ fontconfig 字重刻度。
fn fc_weight(a: &Api, css: u16) -> c_int {
    match a.weight_from_opentype {
        Some(f) => unsafe { f(css as c_int) },
        // 老 fontconfig：按官方常量表分段近似（REGULAR=80、MEDIUM=100、DEMIBOLD=180、BOLD=200）。
        None => match css {
            0..=150 => 0,
            151..=250 => 40,
            251..=350 => 50,
            351..=450 => 80,
            451..=550 => 100,
            551..=650 => 180,
            651..=750 => 200,
            751..=850 => 205,
            _ => 210,
        },
    }
}

/// 按族名 / 字重 / 斜体 / 语言查询，返回 fontconfig 排好序的候选列表（首项即最佳匹配，
/// 其后是按覆盖度去重后的回退链）。
pub(crate) fn sort(
    family: &str,
    weight: u16,
    italic: bool,
    lang: Option<&str>,
) -> Option<Vec<Candidate>> {
    let a = api()?;
    // 族名里的 `-`、`:`、`,` 在 fontconfig 名字语法里有特殊含义，需转义。
    let mut esc = String::new();
    for ch in family.chars() {
        if matches!(ch, '-' | ':' | ',' | '\\') {
            esc.push('\\');
        }
        esc.push(ch);
    }
    let mut query = format!("{esc}:weight={}", fc_weight(a, weight));
    if italic {
        query.push_str(":slant=100");
    }
    if let Some(l) = lang {
        query.push_str(":lang=");
        query.push_str(l);
    }
    let cq = CString::new(query).ok()?;
    unsafe {
        let pat = (a.name_parse)(cq.as_ptr() as *const u8);
        if pat.is_null() {
            return None;
        }
        (a.config_substitute)(a.config, pat, FC_MATCH_PATTERN);
        (a.default_substitute)(pat);
        let mut res: c_int = 0;
        // trim=1：去掉覆盖度不增加的候选，回退链因此短得多。
        let set = (a.font_sort)(a.config, pat, 1, std::ptr::null_mut(), &mut res);
        (a.pattern_destroy)(pat);
        if set.is_null() {
            return None;
        }
        let fs = &*set;
        let mut out = Vec::new();
        for i in 0..fs.nfont.max(0) as usize {
            let p = *fs.fonts.add(i);
            let mut file: *const u8 = std::ptr::null();
            if (a.pattern_get_string)(p, c"file".as_ptr(), 0, &mut file) != FC_RESULT_MATCH
                || file.is_null()
            {
                continue;
            }
            let path = PathBuf::from(
                CStr::from_ptr(file as *const c_char)
                    .to_string_lossy()
                    .into_owned(),
            );
            let mut index: c_int = 0;
            (a.pattern_get_integer)(p, c"index".as_ptr(), 0, &mut index);
            let mut cs: Ptr = std::ptr::null_mut();
            let charset = if (a.pattern_get_charset)(p, c"charset".as_ptr(), 0, &mut cs)
                == FC_RESULT_MATCH
                && !cs.is_null()
            {
                (a.charset_copy)(cs)
            } else {
                std::ptr::null_mut()
            };
            out.push(Candidate {
                path,
                index: index.max(0) as u32,
                charset,
            });
        }
        (a.font_set_destroy)(set);
        Some(out)
    }
}
