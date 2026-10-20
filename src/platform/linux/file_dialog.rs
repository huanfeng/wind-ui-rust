//! `PickDialog` 的 Linux 补丁：分清「对话框没弹出来」与「用户取消」，并在前者时试 kdialog。
//!
//! rfd 的 xdg 后端两头都只回 `None`：门户调用失败时退 zenity，zenity 也没装就记一条日志后
//! 返回 `None`；门户回了非 0 response（比如门户在但没有文件选择后端）也直接 `None`、不试
//! zenity。应用拿到的 `None` 与用户点了取消一模一样，界面静默无反应。rfd 不认 kdialog，
//! 只装了 KDE 那套的机器因此一个选择框也弹不出——这里直接调 kdialog 补上。

use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::platform::PickKind;

/// 返回 `None` 且耗时短于此值即判「没弹出来」。
///
/// 依据：门户调用失败、zenity 不存在都在几十毫秒内返回；而真弹出窗口后用户再取消，
/// 从窗口出现、看清、到按下 Esc / 点取消，人的反应时间就不止 300ms。误判的代价只是
/// 多弹一次 kdialog（或多一条提示），漏判则又回到静默无反应，故阈值取偏低的一侧。
/// 已知漏判：门户在但没有文件选择后端时，D-Bus 激活可能拖到秒级才回失败，仍会当成取消。
pub(crate) const FAIL_FAST: Duration = Duration::from_millis(300);

/// 返回空且这么快，视为对话框根本没出现。
pub(crate) fn looks_failed(elapsed: Duration) -> bool {
    elapsed < FAIL_FAST
}

/// 空列表当 `None`：rfd 多选在门户回非 0 response 时给 `Some(vec![])`，取消也是它，
/// 不折成 `None` 的话多选入口永远判不出「没弹出来」。
pub(crate) fn non_empty(r: Option<Vec<PathBuf>>) -> Option<Vec<PathBuf>> {
    r.filter(|v| !v.is_empty())
}

/// `PickDialog` 链式配置的副本：rfd 的配置不对外可读，kdialog 兜底要自己留一份。
#[derive(Clone, Default)]
pub(crate) struct Spec {
    pub title: Option<String>,
    pub filters: Vec<(String, Vec<String>)>,
    pub directory: Option<PathBuf>,
    pub file_name: Option<String>,
}

/// kdialog 的过滤串：每项 `名称 (*.a *.b)`，多项以换行分隔。KF6 的 kdialog 两种写法都认，
/// 老式 `*.a *.b|名称` 在 Qt 文件框里只显示模式、丢掉名称。
fn filter_arg(filters: &[(String, Vec<String>)]) -> Option<String> {
    let items: Vec<String> = filters
        .iter()
        .filter(|(_, exts)| !exts.is_empty())
        .map(|(name, exts)| {
            let pats: Vec<String> = exts.iter().map(|e| format!("*.{e}")).collect();
            format!("{name} ({})", pats.join(" "))
        })
        .collect();
    (!items.is_empty()).then(|| items.join("\n"))
}

/// 起始位置：保存框把预填文件名拼在目录后；都没给时用家目录（kdialog 的起始参数在过滤串
/// 之前，带过滤串时不能省）。
fn start_arg(spec: &Spec, kind: PickKind, home: Option<&Path>) -> PathBuf {
    let dir = spec
        .directory
        .clone()
        .or_else(|| home.map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    match (&spec.file_name, kind) {
        (Some(name), PickKind::Save) => dir.join(name),
        _ => dir,
    }
}

/// 拼 kdialog 命令行参数（不含程序名）。
pub(crate) fn kdialog_args(spec: &Spec, kind: PickKind, home: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    if let Some(t) = &spec.title {
        args.push("--title".into());
        args.push(t.into());
    }
    let mode = match kind {
        PickKind::File | PickKind::Files => "--getopenfilename",
        PickKind::Save => "--getsavefilename",
        // kdialog 不支持多选目录：多目录退成单选，结果包成一项。
        PickKind::Folder | PickKind::Folders => "--getexistingdirectory",
    };
    args.push(mode.into());
    args.push(start_arg(spec, kind, home).into());
    if matches!(kind, PickKind::File | PickKind::Files | PickKind::Save) {
        if let Some(f) = filter_arg(&spec.filters) {
            args.push(f.into());
        }
    }
    if kind == PickKind::Files {
        args.push("--multiple".into());
        args.push("--separate-output".into());
    }
    args
}

/// kdialog 一次调用的结局。
pub(crate) enum Outcome {
    Picked(Vec<PathBuf>),
    Cancelled,
    /// 没弹出来（没装、起不来、或秒退），附原因供日志。
    Failed(String),
}

/// 按退出码、标准输出与耗时判定结局：0 且有输出 = 选中；1 = 取消（kdialog 的约定），
/// 但秒退的 1 与其它退出码都算没弹出来。
///
/// 输出按字节逐行拆：路径未必是 UTF-8，首尾空格也可能是文件名的一部分，只去行尾 `\r`。
pub(crate) fn classify(code: Option<i32>, stdout: &[u8], elapsed: Duration) -> Outcome {
    match code {
        Some(0) => {
            let paths: Vec<PathBuf> = stdout
                .split(|b| *b == b'\n')
                .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
                .filter(|l| !l.is_empty())
                .map(|l| PathBuf::from(OsStr::from_bytes(l)))
                .collect();
            if paths.is_empty() {
                Outcome::Cancelled
            } else {
                Outcome::Picked(paths)
            }
        }
        Some(1) if !looks_failed(elapsed) => Outcome::Cancelled,
        Some(c) => Outcome::Failed(format!("kdialog 退出码 {c}")),
        None => Outcome::Failed("kdialog 被信号终止".to_string()),
    }
}

/// 阻塞执行 kdialog。
pub(crate) fn run_kdialog(spec: &Spec, kind: PickKind) -> Outcome {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let start = Instant::now();
    let out = Command::new("kdialog")
        .args(kdialog_args(spec, kind, home.as_deref()))
        .output();
    match out {
        Ok(o) => classify(o.status.code(), &o.stdout, start.elapsed()),
        Err(e) => Outcome::Failed(format!("无法执行 kdialog：{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            title: Some("导入方案包".into()),
            filters: vec![
                ("方案包".into(), vec!["wpkg".into(), "zip".into()]),
                ("文本".into(), vec!["txt".into()]),
            ],
            directory: None,
            file_name: None,
        }
    }

    fn strs(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    #[test]
    fn open_file_args_carry_title_start_and_filters() {
        let a = strs(kdialog_args(&spec(), PickKind::File, Some(Path::new("/h"))));
        assert_eq!(
            a,
            [
                "--title",
                "导入方案包",
                "--getopenfilename",
                "/h",
                "方案包 (*.wpkg *.zip)\n文本 (*.txt)",
            ]
        );
    }

    #[test]
    fn multiple_files_ask_for_one_path_per_line() {
        let a = strs(kdialog_args(&spec(), PickKind::Files, None));
        assert_eq!(a[2], "--getopenfilename");
        assert_eq!(a[3], ".", "没有目录也没有家目录时从当前目录起");
        assert_eq!(&a[5..], ["--multiple", "--separate-output"]);
    }

    #[test]
    fn save_joins_file_name_onto_directory() {
        let mut s = spec();
        s.directory = Some("/d".into());
        s.file_name = Some("a.zip".into());
        let a = strs(kdialog_args(&s, PickKind::Save, Some(Path::new("/h"))));
        assert_eq!(a[2], "--getsavefilename");
        assert_eq!(a[3], "/d/a.zip");
        // 打开框不拼文件名
        let a = strs(kdialog_args(&s, PickKind::File, None));
        assert_eq!(a[3], "/d");
    }

    #[test]
    fn folder_ignores_filters_and_title_is_optional() {
        let mut s = spec();
        s.title = None;
        let a = strs(kdialog_args(&s, PickKind::Folders, Some(Path::new("/h"))));
        assert_eq!(a, ["--getexistingdirectory", "/h"]);
    }

    #[test]
    fn empty_extension_lists_are_dropped_from_filter() {
        let mut s = spec();
        s.filters = vec![("空".into(), vec![])];
        let a = strs(kdialog_args(&s, PickKind::File, Some(Path::new("/h"))));
        assert_eq!(a.len(), 4, "无可用过滤项时不传过滤串：{a:?}");
    }

    #[test]
    fn empty_multi_pick_counts_as_none() {
        assert_eq!(non_empty(Some(vec![])), None);
        assert_eq!(non_empty(None), None);
        let one = vec![PathBuf::from("/a")];
        assert_eq!(non_empty(Some(one.clone())), Some(one));
    }

    #[test]
    fn fast_none_is_a_failure_slow_none_is_a_cancel() {
        assert!(looks_failed(Duration::from_millis(20)));
        assert!(!looks_failed(FAIL_FAST));
        assert!(!looks_failed(Duration::from_secs(3)));
    }

    #[test]
    fn classify_by_exit_code_output_and_elapsed() {
        let slow = Duration::from_secs(2);
        let fast = Duration::from_millis(10);
        match classify(Some(0), b"/a/b.zip\n", slow) {
            Outcome::Picked(p) => assert_eq!(p, [PathBuf::from("/a/b.zip")]),
            _ => panic!("应为选中"),
        }
        match classify(Some(0), b"/a\n/b\r\n\n", slow) {
            Outcome::Picked(p) => assert_eq!(p, [PathBuf::from("/a"), PathBuf::from("/b")]),
            _ => panic!("多选应逐行拆开"),
        }
        assert!(matches!(classify(Some(0), b"\n", slow), Outcome::Cancelled));
        assert!(matches!(classify(Some(1), b"", slow), Outcome::Cancelled));
        assert!(matches!(classify(Some(1), b"", fast), Outcome::Failed(_)));
        assert!(matches!(classify(Some(254), b"", slow), Outcome::Failed(_)));
        assert!(matches!(classify(None, b"", slow), Outcome::Failed(_)));
    }

    /// 首尾空格属于文件名；非 UTF-8 字节原样保留。
    #[test]
    fn classify_keeps_spaces_and_non_utf8_bytes() {
        let slow = Duration::from_secs(2);
        match classify(Some(0), b" /a b \n/x\xff\n", slow) {
            Outcome::Picked(p) => {
                assert_eq!(p[0], PathBuf::from(" /a b "));
                assert_eq!(p[1].as_os_str().as_bytes(), b"/x\xff");
            }
            _ => panic!("应为选中"),
        }
    }
}
