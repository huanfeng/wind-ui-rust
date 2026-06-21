# 贡献指南

感谢你对 windui 的关注！欢迎以 issue、讨论、文档、代码等任何形式参与。

> 想深入了解仓库结构、布局算法、如何加控件、平台缝合层等开发细节，请先读
> [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md)。本文只讲**贡献流程**。

## 参与方式

- **报告 Bug**：用 [Issue 模板](.github/ISSUE_TEMPLATE) 提交，附最小复现、平台与版本。
- **提功能建议**：先开 issue 讨论范围与设计，避免大改后方向不符。
- **改文档/示例**：小修可直接 PR。
- **贡献代码**：见下方流程。较大改动请先开 issue 对齐。

## 开发环境

| 平台 | 前置 |
|------|------|
| Windows | Rust（stable）、MSVC 工具链 |
| macOS | Rust（stable）、Xcode Command Line Tools |

```bash
git clone https://github.com/huanfeng/wind-ui-rust
cd wind-ui-rust
cargo run --release --example fullshowcase   # 跑起来看看
```

## 提交前的三道闸

PR 必须在**你改动涉及的平台**上通过以下检查（CI 会在 Windows + macOS 双平台复跑）：

```bash
cargo build --all-targets
cargo test
cargo clippy --all-targets        # 须零警告
```

- 涉及视觉的改动，建议附 `--screenshot` 截图（见 `docs/DEVELOPMENT.md`）。
- **格式**：本项目不强制 `cargo fmt`——图形绘制 API 刻意用较宽的行（与 `#![allow(clippy::too_many_arguments)]` 同源）。请保持与周边代码一致的风格，勿大面积重排无关代码。

## 提交规范

遵循 [Conventional Commits](https://www.conventionalcommits.org/)：

```
<type>(<scope>): <subject>
```

`type` 常用：`feat` / `fix` / `docs` / `refactor` / `test` / `chore`。示例：

```
feat(slider): 支持键盘左右键微调
fix(text): 修复多行光标越界
```

一个提交只做一件事；提交信息用中文或英文均可，与改动范围一致即可。

## DCO 签署（必须）

本项目采用 **Developer Certificate of Origin (DCO)** 而非 CLA：你保留自己贡献的版权，
只需在每个提交里声明「我有权按本项目的开源许可证提交这段代码」。完整条款见 [`DCO`](DCO)。

**怎么做**：提交时加 `-s`（`--signoff`），Git 会自动追加一行 `Signed-off-by`：

```bash
git commit -s -m "fix(text): 修复多行光标越界"
```

生成的提交尾部应包含（用你的真实姓名与邮箱，须与 Git 配置一致）：

```
Signed-off-by: Your Name <your.email@example.com>
```

- 先配置好身份：`git config user.name "Your Name"` 与 `git config user.email "you@example.com"`。
- 忘了签：`git commit --amend -s`（最后一个提交）或 `git rebase --signoff <base>`（整段历史）。
- 多个提交都需签署；CI/检查会拦截未签署的提交。

## Pull Request 流程

1. Fork → 从 `main` 切分支（如 `feat/xxx` / `fix/xxx`）。
2. 改动 + 通过三道闸 + 每个提交 `-s` 签署。
3. 提 PR，按 [PR 模板](.github/PULL_REQUEST_TEMPLATE.md) 填写：动机、做了什么、如何验证、影响平台。
4. 关联相关 issue（`Closes #123`）。
5. 评审中如需修改，追加提交或 `--amend` 后 force-push 到你的分支。

## 许可证

提交即表示你同意：你的贡献按本项目的 **MIT OR Apache-2.0** 双许可授权发布，无附加条款。

## 行为准则

参与本项目即表示你同意遵守 [行为准则](CODE_OF_CONDUCT.md)。请保持友善、尊重与建设性，对事不对人。
