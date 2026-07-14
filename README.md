# DeepSeek CLI

[中文](README.zh-CN.md)

A stateless DeepSeek subagent CLI and an Agent Skill for delegating bounded consultations and safe independent implementations. The CLI gives the model local shell capabilities; the caller remains responsible for scope, safety, and verification.

The CLI currently pins the model ID to `deepseek-v4-flash` (DeepSeek V4 Flash).

## Install

The skill and CLI are installed separately.

```sh
npx skills add model-clis/deepseek
```

Windows (preferred):

```powershell
scoop bucket add model-clis https://github.com/model-clis/homebrew-packages
scoop install model-clis/deepseek
```

Windows without Scoop:

```powershell
irm https://raw.githubusercontent.com/model-clis/deepseek/main/scripts/install.ps1 | iex
```

Apple Silicon macOS (preferred):

```sh
brew tap model-clis/packages
brew install deepseek
```

Linux x64 or macOS Apple Silicon without Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/model-clis/deepseek/main/scripts/install.sh | sh
```

Installers fetch only GitHub Release assets, verify SHA-256, and default to `~/.local/bin`. Set `DEEPSEEK_INSTALL_DIR` to change the destination or a complete tag such as `DEEPSEEK_VERSION=v2026.714.0` to pin a release. Explicit installer runs may replace an existing binary; the skill never installs or upgrades silently.

## Login and usage

Configure the API key privately in your own terminal—never paste it into a chat:

```sh
deepseek login
deepseek "Investigate this issue read-only and report evidence" --max-turns 128
deepseek --prompt-file /absolute/path/prompt.txt --delete-prompt-file --max-turns 128
```

The CLI is stateless and has no consultation/execution mode. Prompts must be self-contained. Stdout contains the final report; stderr is diagnostic. Exit codes are `0` normal, `1` failure, `2` valid but incomplete, and `130` interrupted; callers must interpret results and inspect the workspace rather than automate solely from a code.

## Tools and safety

The model can use local shell tools, but the CLI is not a permission sandbox. Never delegate execution involving destructive deletion, Git push/history/tags, releases/deployment, production/shared infrastructure, database migration or bulk changes, credentials, permissions/security/billing, external communication, or administrator/root privileges. Read-only consultation about sensitive areas must explicitly prohibit side effects. See [`skills/delegating-to-deepseek`](skills/delegating-to-deepseek/SKILL.md) for decomposition, concurrency, and prompt templates.

## Development

Requires Rust 1.85 or newer.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
npx skills add . --list
sh -n scripts/install.sh
```

## Releases

The automated daily workflow publishes only when `main` has commits not present in the latest release. Versions use Hong Kong dates (`vYYYY.MDD.REV`). Stable assets are produced for Windows x64, Linux x64 (musl), and macOS Apple Silicon, each with a `.sha256` file. There are no nightly or prerelease channels.

Homebrew and Scoop metadata is maintained in [`model-clis/homebrew-packages`](https://github.com/model-clis/homebrew-packages) and reconciled with the latest release daily.

Repository: <https://github.com/model-clis/deepseek>

## License

MIT
