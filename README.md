<div align="center">

# goose

_your native open source AI agent — desktop app, CLI, and API — for code, workflows, and everything in between_

<p align="center">
  <a href="https://opensource.org/licenses/Apache-2.0"
    ><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg"></a>
  <a href="https://discord.gg/n8R5VaWDAn"
    ><img src="https://img.shields.io/discord/1287729918100246654?logo=discord&logoColor=white&label=Join+Us&color=blueviolet" alt="Discord"></a>
  <a href="https://github.com/aaif-goose/goose/actions/workflows/ci.yml"
     ><img src="https://img.shields.io/github/actions/workflow/status/aaif-goose/goose/ci.yml?branch=main" alt="CI"></a>
  <a href="https://insights.linuxfoundation.org/project/goose"><img src="https://insights.linuxfoundation.org/api/badge/health-score?project=goose"></a>
  <a href="https://repology.org/project/goose-cli/versions"><img src="https://repology.org/badge/tiny-repos/goose-cli.svg" alt="Packaging status"></a>
</p>

<a href="https://trendshift.io/repositories/25298?utm_source=repository-badge&amp;utm_medium=badge&amp;utm_campaign=badge-repository-25298" target="_blank" rel="noopener noreferrer"><img src="https://trendshift.io/api/badge/repositories/25298" alt="aaif-goose%2Fgoose | Trendshift" width="250" height="55"/></a>

</div>


goose is a general-purpose AI agent that runs on your machine. Not just for code — use it for research, writing, automation, data analysis, or anything you need to get done.

A native desktop app for macOS, Linux, and Windows. A full CLI for terminal workflows. An API to embed it anywhere. Built in Rust for performance and portability.

goose works with 15+ providers — Anthropic, OpenAI, Google, Ollama, OpenRouter, Azure, Bedrock, and more. Use API keys or your existing Claude, ChatGPT, or Gemini subscriptions via [ACP](https://goose-docs.ai/docs/guides/acp-providers). Connect to 70+ extensions via the [Model Context Protocol](https://modelcontextprotocol.io/) open standard.

goose is part of the [Agentic AI Foundation (AAIF)](https://aaif.io/) at the Linux Foundation.

# Get started

**[Download the desktop app](https://goose-docs.ai/docs/getting-started/installation)** for macOS, Linux, and Windows.

Install or update the ViaTech CLI on macOS, Linux, WSL, Android/Termux, Git
Bash, or MSYS2 with the checksum-enforcing release installer:

```bash
curl -fsSL https://github.com/ViaTechSystems/goose/releases/download/stable/download_cli.sh | bash
```

If the [ViaTech releases page](https://github.com/ViaTechSystems/goose/releases)
does not yet show a `stable` release with binary and `.sha256` assets, or for a
native PowerShell install, build the current CLI from source with Rust/Cargo:

```bash
cargo install --force --git https://github.com/ViaTechSystems/goose goose-cli \
  --locked --no-default-features --features rustls-tls,code-mode
```

The CLI does not update itself in the background. For now, update a source
install by rerunning the `cargo install` command above; that minimal source build
does not include `goose update`. For a packaged install, rerun the download
command (it rejects a missing, malformed, or mismatched SHA-256 sidecar) or run
`goose update`. The packaged
updater uses the ViaTech release channel and fails closed unless the downloaded
archive has valid Sigstore/SLSA provenance.

## ViaTech terminal controls

The fork's interactive CLI adds coding-session controls directly to goose:

- `/model` and `/think` switch models and reasoning effort; `Shift+Tab` cycles
  the supported reasoning levels without losing the draft.
- `/permissions`, `/pwd`, and `/cd` expose approval and workspace state. An
  ExactCode-governed session cannot use `/cd` or symlinks to escape its
  authorized workspace.
- `/new`, `/resume`, `/fork`, `/rename`, and `/sessions` manage durable sessions;
  `/diff`, `/review`, `/goal`, `/queue`, and `/status` expose coding workflow
  state.
- A private checkpoint is captured before each submitted prompt and direct
  `/agent` delegation. `/rewind` can restore the conversation, Git-backed code,
  or both after confirmation, or create a non-destructive session fork from an
  earlier checkpoint.
- `/image` queues validated PNG, JPEG, or WebP attachments for the next turn;
  `/images` inspects or clears them.
- `/subagents`, `/agent`, `/ps`, and `/stop` manage governed parallel work and
  background processes.
- While a response streams, `Enter` steers the active turn and `Tab` queues a
  distinct next turn. `Ctrl+J` inserts a newline, and `/edit` opens the prompt
  editor.

In an ExactCode-governed session, repository-provided goose plugins, command
hooks, and plugin MCP servers are blocked by default. An operator can explicitly
trust them for that launch with `EXACTCODE_TRUST_PROJECT_PLUGINS=1`; plugin hook
commands then run with the user's operating-system permissions and must be
reviewed as executable code.

See the [CLI command guide](https://goose-docs.ai/docs/guides/goose-cli-commands#interactive-session-features)
for command arguments, limits, and key behavior.

# Quick links
- [Quickstart](https://goose-docs.ai/docs/quickstart)
- [Installation](https://goose-docs.ai/docs/getting-started/installation)
- [Tutorials](https://goose-docs.ai/docs/category/tutorials)
- [Documentation](https://goose-docs.ai/docs/category/getting-started)
- [Governance](https://github.com/aaif-goose/goose/blob/main/GOVERNANCE.md)
- [Custom Distributions](https://github.com/aaif-goose/goose/blob/main/CUSTOM_DISTROS.md) — build your own goose distro with preconfigured providers, extensions, and branding

## Need help?
- [Diagnostics & Reporting](https://goose-docs.ai/docs/troubleshooting/diagnostics-and-reporting)
- [Known Issues](https://goose-docs.ai/docs/troubleshooting/known-issues)

# a little goose humor 🪿

> Why did the developer choose goose as their AI agent?
> 
> Because it always helps them "migrate" their code to production! 🚀

# goose around with us
- [Discord](https://discord.gg/n8R5VaWDAn)
- [YouTube](https://www.youtube.com/@goose-oss)
- [LinkedIn](https://www.linkedin.com/company/goose-oss)
- [Twitter/X](https://x.com/goose_oss)
