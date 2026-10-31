# Agent2WebProviders

[English](README.md) · **简体中文**

**网页提供商**是 [Agent2API](https://github.com/aimod-cc/Agent2API) 的官方网页提供商插件。
它将智谱清言与 DeepSeek 网页账号接入网关的统一模型接口。
网关以独立进程运行插件，统一管理账号、路由与请求报表。

具体能力取决于账号与适配实现。本插件使用网页账号凭证，不使用提供商的收费 API Key。
网关安装与部署说明见 [Agent2API 安装文档](https://github.com/aimod-cc/Agent2API/blob/main/docs/install.md)。

## 支持的提供商

下表描述当前实现，不代表所有能力都已通过真实账号验证。

| 提供商 | LLM 请求 | Token 自动续期 | 模型列表 | 余额查询 | 签到 | 领取类 |
| --- | :--: | :--: | :--: | :--: | :--: | :--: |
| 智谱清言 | ✓ | ✓ | 在线目录 + 静态兜底 | 仅登录账号 | 仅登录账号 | 仅登录账号 |
| DeepSeek 网页 | 文本与图片输入；不支持工具调用 | 未声明 | 在线配置 + 静态兜底 | 未声明 | 未声明 | 未声明 |

「未声明」表示该能力尚未确认或接入，不等于上游没有此能力。

### 智谱清言

- 支持访客模式、粘贴令牌与 Windows 本机登录态导入。
- 支持在线拉取模型目录，另有内置静态清单兜底。
- 登录账号有刷新令牌时，可自动续期凭证。
- 访客可对话并换发凭证，但没有积分、签到与奖励体系。
- 积分查询、每日签到、会员活动与任务领取仅对登录账号有效。
- 已有账号沿用提供商标识 `chatglm`。

### DeepSeek 网页

- 使用本人 `chat.deepseek.com` 的 `userToken.value`，必要时附上 Cookie。
- 仅展示一个模型 `deepseek-web-flash`（展示名：DeepSeek 网页），从官方账号配置选择当前默认模式；已关闭的专家模式与识图模式不再列为独立模型。
- 支持流式正文、思考内容与真实图片上传，文件解析成功后通过 `ref_file_ids` 传入对话。
- 图片支持 PNG、JPEG、GIF、WebP，可使用 base64 data URL 或公网 HTTP(S) URL；拒绝本机、内网地址及重定向，下载图片时不发送账号凭证。
- 网页接口没有原生工具调用能力，已移除提示词模拟；启用 `tools` 或传入工具历史时明确报错。
- 不接受收费 API Key；遇到人机验证须在官方网页手动登录。
- 已使用真实账号验证文本、流式回复、模型配置和图片形状、颜色、文字识别；续期、余额、签到与领取仍未声明支持。

令牌、Cookie 与账号导出均属于敏感凭证，请勿公开或分享。

## 开发与构建

需要 Rust 1.88 或更高版本，打包脚本另需 Node.js。

构建并打包当前平台：

```bash
cargo build --release
node scripts/pack.mjs --skip-build
```

产物为 `dist/agent2api-web-providers-<version>-<target>.zip`。
包内包含 `pack.json`、可执行文件、图标与提供商能力说明。

构建其他平台时，先准备对应工具链：

```bash
cargo build --release --target aarch64-unknown-linux-gnu
node scripts/pack.mjs --skip-build --target aarch64-unknown-linux-gnu
```

每个平台都需要独立的原生可执行文件，清单声明了以下目标：

| 目标三元组 | 平台 |
| --- | --- |
| `x86_64-pc-windows-msvc` | Windows x64 |
| `x86_64-apple-darwin` | macOS Intel |
| `aarch64-apple-darwin` | macOS Apple Silicon |
| `x86_64-unknown-linux-gnu` | Linux / Docker amd64 |
| `aarch64-unknown-linux-gnu` | Linux / Docker arm64 |

## 仓库结构

- [`crates/providers/`](crates/providers/)：提供商实现与插件可执行程序。
- [`pack.json`](pack.json)：插件标识、版本、提供商清单与平台入口。
- [`ui/provider-docs.json`](ui/provider-docs.json)：六种语言的能力说明。
- [`assets/`](assets/)：提供商图标。
- [`scripts/pack.mjs`](scripts/pack.mjs)：原生插件包打包脚本。

共享的 `agent2api-provider-protocol` 与 `agent2api-provider-sdk` 来自 [Agent2Providers](https://github.com/ProvidersPlugin/Agent2Providers)。
具体 Git tag 引用见 [`crates/providers/Cargo.toml`](crates/providers/Cargo.toml)。

## 许可

[GNU Affero General Public License v3.0（AGPL-3.0）](LICENSE)。
