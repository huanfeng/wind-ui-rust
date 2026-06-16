# 一键生成所有示例截屏，用于自动化回归比对。
# 用法：powershell scripts/screenshots.ps1
$ErrorActionPreference = "Stop"
$out = "artifacts"
New-Item -ItemType Directory -Force -Path $out | Out-Null

# 示例名 -> 额外参数
$shots = @{
    "phase0_window"     = @()
    "phase1_layout"     = @()
    "phase2_text"       = @()
    "phase3_button"     = @()
    "phase4_form"       = @()
    "phase5_containers" = @()
    "fullshowcase"      = @()
}

foreach ($name in $shots.Keys) {
    $png = Join-Path $out "$name.png"
    Write-Host "==> $name"
    & cargo run --quiet --example $name -- --screenshot $png @($shots[$name])
}

# 带对话框的变体
& cargo run --quiet --example phase5_containers -- --dialog --screenshot (Join-Path $out "phase5_dialog.png")
& cargo run --quiet --example fullshowcase -- --dialog --screenshot (Join-Path $out "showcase_dialog.png")

Write-Host "`n截屏已全部生成于 $out/"
Get-ChildItem $out -Filter *.png | Select-Object Name, @{N="KB";E={[math]::Round($_.Length/1KB,1)}}
