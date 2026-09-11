//! 把 `assets/windui.ico` 编译期嵌入**示例程序**的 exe 资源。
//!
//! 嵌进去之后，窗口类的 `LoadIconW`（见 `platform/win32` 的 `register_window_class`）
//! 就能取到多尺寸 `.ico`：标题栏 16px、任务栏 32px、Alt-Tab 48px 各取所需一档，
//! 比运行时 `App::icon` 交给系统缩放的单尺寸位图锐利，exe 文件在资源管理器里也带图标。
//! 两条路互补，示例两条都走。
//!
//! ## ⚠️ 必须用 `compile_for_examples`，不能用 `compile`
//!
//! 目标是「只烙在示例 exe 上」，而 `embed_resource::compile()` **对纯库 crate 做不到这件事**：
//! 它先探测本 crate 有没有 bin（`[[bin]]` / `src/main.rs` / `src/bin/`），有才发
//! `cargo:rustc-link-arg-bins`；windui 没有 bin，于是落进 pre-0.51 兼容分支，改发
//! `cargo:rustc-link-search` + `cargo:rustc-link-lib`——**那两条是会传递给下游的**，
//! 于是每个链接 windui 的 exe 都被塞进这张图标。
//!
//! 后果不是「多一张图标」而是**下游直接链接失败**：下游若自带 `.ico`，两边的 RT_ICON
//! 都由资源编译器从 id 1 起编号，跨 `.res`/`.lib` 不统一编号，CVTRES 报
//! `CVT1100: 资源重复。类型: ICON，名称: 1` 而后 LNK1123。wind-setting 就是这么倒下的，
//! 且**下游改自己的图标 id 治不了**（那改的是 GROUP_ICON 的 id，冲突在 RT_ICON 那一层）。
//!
//! `compile_for_examples` 发的是 `cargo:rustc-link-arg-examples`，只作用于本 package 的
//! examples，下游一无所知——这才是这段代码的原意。
//!
//! 图标资源由 `scripts/gen_icon.py` 生成。资源文件缺失时（如精简的打包场景）静默跳过，
//! 不让缺一张图标阻断构建。

fn main() {
    println!("cargo:rerun-if-changed=assets/windui.rc");
    println!("cargo:rerun-if-changed=assets/windui.ico");

    emit_gpu_cfg();

    // host 非 Windows 时 embed-resource 根本没被拉进来（见 Cargo.toml 的
    // `target.'cfg(windows)'.build-dependencies`），故用 host cfg 门控整段。
    #[cfg(windows)]
    embed_icon();
}

#[cfg(windows)]
fn embed_icon() {
    use std::path::Path;

    // host 是 Windows 但交叉编译到别的平台时，Win32 资源无从谈起。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let rc = Path::new("assets/windui.rc");
    if !rc.exists() || !Path::new("assets/windui.ico").exists() {
        return;
    }

    // `manifest_optional`：资源编译器（rc.exe / llvm-rc）不在时降级为警告而非硬失败。
    // 没有它，一台没装 Windows SDK 的机器会连库都构建不了——为一张图标不值当。
    //
    // ⚠️ `compile_for_examples` 而非 `compile`：理由见模块头，改回去会让所有自带图标的
    // 下游 exe 链接失败。
    if let Err(e) =
        embed_resource::compile_for_examples(rc, embed_resource::NONE).manifest_optional()
    {
        println!("cargo:warning=示例图标资源嵌入跳过：{e}");
    }
}

/// 发出 `cfg(gpu_backend)`——GPU 后端（`src/render/gpu/`）与 macOS 的 `CAMetalLayer` 接线
/// 统一用它门控，而不是直接写 `feature = "gpu"`。
///
/// 为什么要这层别名：目标是「macOS 恒编入 GPU 后端，其余平台按 feature 决定」，而
/// **Cargo 没有按 target 生效的默认 feature**——`default` 是全局的，把 `gpu` 写进去
/// Windows 也会跟着编一棵 wgpu 依赖树。依赖侧的解法是分 target 段声明（macOS 非
/// optional、其余 optional 挂 feature，见 Cargo.toml），源码侧就需要一个能同时表达两者
/// 的条件——即本函数发出的这个 cfg。
///
/// 判据与 Cargo.toml 的依赖声明**必须一一对应**：两边任一边改了而另一边没改，症状是
/// 「引用了没引入的 crate」或「引入了没人用的 crate」，前者编译不过、后者只是多编。
fn emit_gpu_cfg() {
    // 未声明的 cfg 在新版 rustc 下会报 `unexpected_cfgs` 警告，而本项目 CI 以
    // `-D warnings` 跑 clippy——不声明这一行，整个门禁会因为一个自定义 cfg 全红。
    println!("cargo::rustc-check-cfg=cfg(gpu_backend)");
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    // `CARGO_FEATURE_GPU` 是 Cargo 为开启的 feature 注入的环境变量（名字规则：大写、
    // 连字符转下划线）。
    let feature = std::env::var_os("CARGO_FEATURE_GPU").is_some();
    if macos || feature {
        println!("cargo::rustc-cfg=gpu_backend");
    }
}
