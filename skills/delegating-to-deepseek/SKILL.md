---
name: delegating-to-deepseek
description: "Delegates bounded consultation or safe, independent implementation to DeepSeek. Use when the user explicitly requests DeepSeek or a second model, or when the caller decides a clearly bounded consultation or safe independent implementation should go to DeepSeek; do not trigger automatically for ordinary tasks."
compatibility: "Requires the deepseek CLI, network access to the DeepSeek API, and an API key configured by the user with deepseek login. The caller's shell tool must support invoking the CLI."
---

# Delegating to DeepSeek

Use the stateless `deepseek` CLI as a subagent. The CLI has no modes: first decide whether the work is read-only consultation or safe execution, then load exactly the corresponding template:

- Read `references/consult-prompt.md` for investigation, review, analysis, or planning with no side effects.
- Read `references/execute-prompt.md` for a safe, independent implementation.

Build a fully self-contained ordinary prompt. Never tell DeepSeek to use “consult mode” or any other nonexistent mode.

## Safety gate

The caller—not the CLI—must classify the task. The CLI provides no permission isolation.

Never delegate execution involving irreversible deletion; push, history rewriting, or tags; releases, packaging, or deployment; production or shared infrastructure; database bulk changes or migrations; credentials; permissions, security controls, or billing; external communication; or admin/root access. A read-only consultation about these areas is allowed only when the prompt explicitly requires no side effects.

If execution crosses this boundary, refuse delegation. Do not weaken the boundary by asking DeepSeek to “be careful.”

## Scope and decomposition

Split large work into bounded, stateless calls. Read-only calls are usually safe to run concurrently. Execute calls may run concurrently only when their file scopes do not overlap. Never concurrently delegate work touching the same file, formatting, dependencies, Git, or code generation. The caller must synthesize all reports and inspect the workspace afterward.

For external context use absolute paths and this exact shape:

```xml
<references>
  <reference path="/absolute/path/to/item" purpose="why it is relevant" />
</references>
```

Paths may identify files or directories; every entry needs a purpose. Put required reads in completion criteria as well. Save URL content locally first and reference its absolute path.

## Install and authenticate

Check the version and invoke in one shell-tool invocation to reduce tool calls. If the CLI is absent, explain that installation is required and obtain user confirmation before installing. Do not silently overwrite or auto-upgrade it.

Preferred Windows installation:

```powershell
scoop bucket add model-clis https://github.com/model-clis/homebrew-packages
scoop install model-clis/deepseek
```

Without Scoop, use:

```powershell
irm https://raw.githubusercontent.com/model-clis/deepseek/main/scripts/install.ps1 | iex
```

On Apple Silicon macOS with Homebrew:

```sh
brew tap model-clis/packages
brew install deepseek
```

On Linux, or macOS without Homebrew:

```sh
curl -fsSL https://raw.githubusercontent.com/model-clis/deepseek/main/scripts/install.sh | sh
```

After user-approved installation, check the version. Do not preflight the API key separately: invoke first. If authentication fails, ask the user to run `deepseek login` in their own terminal. Never ask them to paste an API key into the conversation.

## Invoke

Short prompts may be positional:

```sh
command -v deepseek >/dev/null 2>&1 && deepseek --version && deepseek 'fully self-contained prompt' --max-turns 128
```

Normally write a randomly named UTF-8 prompt under the OS temporary directory, pass its absolute path, and delete it through the CLI:

```sh
command -v deepseek >/dev/null 2>&1 && deepseek --version && deepseek --prompt-file "$prompt_file" --delete-prompt-file --max-turns 128
```

PowerShell 5-compatible pattern (do not use `&&`):

```powershell
$cmd = Get-Command deepseek -ErrorAction SilentlyContinue
if ($cmd) { deepseek --version; if ($LASTEXITCODE -eq 0) { deepseek --prompt-file $promptFile --delete-prompt-file --max-turns 128 } }
```

Retain the prompt only when auditability requires it. Never put credentials in a prompt file. Stdout is the final report; stderr normally need not be examined unless diagnosing a failure.

Exit codes are signals for caller orchestration, not hard-coded automatic decisions: `0` normal, `1` failure, `2` valid but incomplete, and `130` interrupted. Independently evaluate the report and workspace.

## Completion

Continue until every completion criterion is met, or a genuine blocker or safety boundary prevents completion. Require truthful reporting of incomplete work. Final-response templates deliberately contain no recommendations field.
