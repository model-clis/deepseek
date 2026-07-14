# DeepSeek CLI

[English](README.md)

无状态 DeepSeek 子代理 CLI，以及用于委托边界清晰的只读咨询和安全独立实现的 Agent Skill。CLI 为模型提供本地 shell 能力；调用方始终负责范围、安全判断和结果验证。

## 安装

Skill 与 CLI 需要分别安装。

```sh
npx skills add model-clis/deepseek
```

Windows（推荐）：

```powershell
scoop bucket add model-clis https://github.com/model-clis/homebrew-packages
scoop install model-clis/deepseek
```

Windows 无 Scoop：

```powershell
irm https://raw.githubusercontent.com/model-clis/deepseek/main/scripts/install.ps1 | iex
```

Apple Silicon macOS（推荐）：

```sh
brew tap model-clis/packages
brew install deepseek
```

Linux x64 或无 Homebrew 的 Apple Silicon macOS：

```sh
curl -fsSL https://raw.githubusercontent.com/model-clis/deepseek/main/scripts/install.sh | sh
```

安装器仅下载 GitHub Release 资产，严格校验 SHA-256，默认安装到 `~/.local/bin`。可用 `DEEPSEEK_INSTALL_DIR` 更改目录，或用完整标签（如 `DEEPSEEK_VERSION=v2026.714.0`）固定版本。显式运行安装脚本可能覆盖已有二进制；Skill 绝不会静默安装或升级。

## 登录与用法

请在自己的终端私下配置 API key，绝不要粘贴到对话中：

```sh
deepseek login
deepseek "只读调查此问题并报告证据" --max-turns 128
deepseek --prompt-file /absolute/path/prompt.txt --delete-prompt-file --max-turns 128
```

CLI 无状态，也没有咨询/执行模式。Prompt 必须完全自包含。stdout 是最终报告，stderr 用于诊断。退出码：`0` 正常、`1` 失败、`2` 有效但未完成、`130` 中断；调用方必须自行解释结果并检查工作区，不能只依赖退出码自动决策。

## 工具与安全

模型可以使用本地 shell 工具，但 CLI 不是权限沙箱。绝不可委托涉及破坏性删除、Git push/history/tag、发布/部署、生产或共享基础设施、数据库迁移或批量操作、凭据、权限/安全/计费、外部通信、管理员/root 权限的执行任务。敏感领域的只读咨询必须明确禁止副作用。分解、并发和 prompt 模板见 [`skills/delegating-to-deepseek`](skills/delegating-to-deepseek/SKILL.md)。

## 开发

需要 Rust 1.85 或更高版本。

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
npx skills add . --list
sh -n scripts/install.sh
```

## 发布

每日自动流程仅在 `main` 存在尚未包含在最新 Release 的提交时发布。版本使用香港日期（`vYYYY.MDD.REV`）。稳定资产覆盖 Windows x64、Linux x64（musl）和 Apple Silicon macOS，每个资产都有 `.sha256`。不提供 nightly 或 prerelease。

Homebrew 与 Scoop 元数据由 [`model-clis/homebrew-packages`](https://github.com/model-clis/homebrew-packages) 统一维护，并每日与最新 Release 对齐。

仓库：<https://github.com/model-clis/deepseek>

## 许可证

MIT
