# 生成 README / README.en 用的界面配图，输出到 docs/images/。
#
# 用法：powershell scripts/readme_shots.ps1
#
# 与 screenshots.ps1 的分工：那个是**回归比对**用的（phase0–5 全量截屏，比对像素差异），
# 这个是**文档配图**用的（只截 README 摆出来的那几张，尺寸与点击状态都调过）。
#
# ⚠ 关于 --click 坐标：它们是**逻辑像素**，且依赖示例当前的布局。改了示例的窗口尺寸、
# 侧栏宽度或标题栏高度，这里的坐标就会打偏——生成后请**逐张看一眼**，不要只看命令成功。
# 每条都标了它想点中的是什么，坐标失效时按注释重新量。
$ErrorActionPreference = "Stop"
$out = "docs/images"
New-Item -ItemType Directory -Force -Path $out | Out-Null

function Shot($name, $file, $extra) {
    Write-Host "==> $file"
    $png = Join-Path $out "$file.png"
    & cargo run --quiet --release --example $name -- --screenshot $png @extra
    if ($LASTEXITCODE -ne 0) { throw "示例 $name 截屏失败" }
}

# hero：设置窗的「输入」页 —— 一屏里 segmented / stepper / dropdown / slider / switch / chip 都在。
# 点的是左侧栏第二项「输入」（侧栏 x≈60，第二项 y≈152）。
Shot "settings"     "settings-input"  @("--click", "60", "152")

# 设置窗的模态对话框：点右上角「标点表格」按钮（x≈890, y≈78）。
Shot "settings"     "settings-dialog" @("--click", "890", "78")

# 控件总览：默认停在「表单」页。
Shot "fullshowcase" "fullshowcase"    @()

# 主题：点顶部第三枚「海洋」按钮（x≈780, y≈73），截 TOML 自定义主题生效后的样子。
Shot "theming"      "theming"         @("--click", "780", "73")

# 其余三张都是打开即所见，无需交互。
Shot "virtual_list" "virtual-list"    @()
Shot "image"        "image"           @()
Shot "about"        "about"           @()

Write-Host "`nREADME 配图已生成于 $out/"
Get-ChildItem $out -Filter *.png | Select-Object Name, @{N = "KB"; E = { [math]::Round($_.Length / 1KB, 1) } }
