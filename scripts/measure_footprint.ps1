# 量 README 性能表里的两个数字：release 二进制体积 与 运行期私有内存。
#
# 用法：powershell scripts/measure_footprint.ps1
#
# 为什么要有这个脚本：这两个数字此前只写在 README 里、没写测的是哪个二进制、
# 更没写在什么 DPI 下测的，于是示例一改尺寸就没人能核对它还准不准。
#
# 会短暂弹出示例窗口（每个约 4 秒）后自动关闭 —— 私有内存必须在**正常运行态**下取，
# 离屏截图模式不经过窗口创建与呈现链路，量不到真实占用。
#
# ⚠ 内存数字**离开 DPI 就没有意义**。软件光栅要为窗口留约 2.5 份全屏 RGBA 缓冲，
# 而缓冲按**物理**像素分配：200% 缩放下同一窗口的物理面积是 100% 的 4 倍。
# 同一个 about 示例，100% 下 5.5MB、200% 下 14.7MB —— 两个都对，只是环境不同。
# 所以这里两种模式各测一遍：DPI 感知（本机真实缩放）与 DPI-unaware（等效 100%）。
# 后者靠 __COMPAT_LAYER=DPIUNAWARE 这个系统 shim：它在进程启动时就把感知级别定死，
# 代码里那句 SetProcessDpiAwarenessContext 随后会失败（返回值被 `let _ =` 丢掉），
# 于是进程按 96 DPI 渲染、由系统拉伸 —— 正是我们要的 100% 等效基准。
$ErrorActionPreference = "Stop"

# 基准二进制：从最小窗口到全功能演示，覆盖体积/内存的两端。
$targets = @(
    @{ Name = "phase0_window"; Label = "最小窗口应用（480x320）" },
    @{ Name = "about"; Label = "关于窗（620x556，典型小工具）" },
    @{ Name = "theming"; Label = "主题窗（880x700）" },
    @{ Name = "settings"; Label = "设置窗（1040x700，侧栏+表格+对话框）" },
    @{ Name = "virtual_list"; Label = "虚拟滚动（10 万行数据）" },
    @{ Name = "fullshowcase"; Label = "综合示例（全控件 + SVG + 10 万行）" }
)

Write-Host "==> 构建 release（opt-level=z + LTO + strip，见 Cargo.toml [profile.release]）"
foreach ($t in $targets) {
    & cargo build --quiet --release --example $t.Name
    if ($LASTEXITCODE -ne 0) { throw "构建 $($t.Name) 失败" }
}

$rows = @()
foreach ($t in $targets) {
    $exe = "target/release/examples/$($t.Name).exe"
    if (-not (Test-Path $exe)) { throw "找不到 $exe" }
    $sizeMB = [math]::Round((Get-Item $exe).Length / 1MB, 2)

    $row = [ordered]@{ 示例 = $t.Name; 说明 = $t.Label; 二进制MB = $sizeMB }

    foreach ($mode in @("感知", "等效100")) {
        if ($mode -eq "等效100") { $env:__COMPAT_LAYER = "DPIUNAWARE" }
        Write-Host "==> $($t.Name) [$mode] 采样内存…"
        $p = Start-Process -FilePath $exe -PassThru
        try {
            # 头一两秒还在建窗口 / 装字体缓存，等它落定再取，否则读到的是爬升中的值。
            Start-Sleep -Seconds 3
            $samples = @()
            for ($i = 0; $i -lt 3; $i++) {
                $p.Refresh()
                $samples += [pscustomobject]@{ Private = $p.PrivateMemorySize64; Working = $p.WorkingSet64 }
                Start-Sleep -Milliseconds 400
            }
        }
        finally {
            if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
            if ($mode -eq "等效100") { Remove-Item Env:__COMPAT_LAYER -ErrorAction SilentlyContinue }
        }
        # 取中位数：单次采样会被后台的一次 GC/分配抖到。
        $row["私有$mode"] = [math]::Round((($samples.Private | Sort-Object)[1]) / 1MB, 2)
        $row["工作集$mode"] = [math]::Round((($samples.Working | Sort-Object)[1]) / 1MB, 2)
    }
    $rows += [pscustomobject]$row
}

Write-Host ""
$rows | Format-Table -AutoSize
Write-Host "私有内存 = Process.PrivateMemorySize64（对应性能计数器 Private Bytes）"
Write-Host "工作集   = Process.WorkingSet64（含 gdi32/dwrite 等跨进程共享的系统 DLL 映射）"
Write-Host "「感知」= 本机真实 DPI 缩放；「等效100」= DPIUNAWARE shim 下按 96 DPI 渲染。"
