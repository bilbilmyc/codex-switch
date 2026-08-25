# Codex Switch

Codex Switch 是一个轻量的 Codex 中转站配置切换工具，使用 Rust 和 Slint 构建，支持 macOS 与 Windows。它只管理中转站、API key、默认模型和可选的审查模型，不试图替代 Codex 的完整配置界面。

## 功能范围

- 保存多个中转站配置，并通过显式的“应用”操作切换当前配置。
- 从当前 `~/.codex/config.toml` 和 `~/.codex/auth.json` 导入可兼容的中转站。
- 每个中转站分别记住默认模型和可选的 `review_model`。
- 使用 Bearer API key 请求中转站的标准 `GET /models` 接口，缓存模型列表；接口不可用时仍可手动填写模型 ID。
- 导入和导出工具配置。默认导出不包含 API key，只有明确选择后才会带出密钥。
- 应用前保存备份，保留最近 10 份，并提供恢复入口。
- 检测 Codex 相关配置被外部修改的冲突，以及正在运行的 Codex CLI 或桌面应用。

本工具只支持 `auth.json` 中的 `OPENAI_API_KEY` 认证和 Codex 的 Responses wire API。它不管理 OAuth 登录、多种鉴权字段、系统代理、Codex 原生模型目录、托盘常驻、自动更新或完整的 Codex 设置。

## 数据与隐私

工具的数据统一保存在用户家目录的隐藏目录 `~/.codex-switch`：

| 路径 | 内容 |
| --- | --- |
| `~/.codex-switch/profiles.toml` | 中转站、模型和 API key |
| `~/.codex-switch/state.json` | 当前应用状态与冲突检测信息 |
| `~/.codex-switch/model-cache/` | 各中转站最近获取的模型列表 |
| `~/.codex-switch/backups/` | 应用配置前生成的 Codex 配置备份 |

这些文件不会额外加密，`profiles.toml` 和备份中的 API key 都是明文。macOS/Unix 上工具会尽量将目录和文件权限分别限制为 `0700` 和 `0600`；Windows 上仍应依赖当前用户账户与磁盘权限保护家目录。不要把 `~/.codex-switch` 提交到 Git、上传网盘或发送给其他人。

## 写入 Codex 的范围

应用中转站时，工具对 `~/.codex/config.toml` 做保留格式的局部修改，只维护以下字段：

```toml
model_provider = "codex_switch"
model = "中转站默认模型"
# 启用审查模型时才存在：
review_model = "中转站审查模型"

[model_providers.codex_switch]
name = "中转站名称"
base_url = "https://relay.example/v1"
wire_api = "responses"
requires_openai_auth = true
```

同时只更新 `~/.codex/auth.json` 的 `OPENAI_API_KEY`。其他 Codex 配置、其他 provider 和 `auth.json` 中的其他字段会保留。禁用审查模型时，工具会移除自己管理的顶层 `review_model`。

模型刷新会把中转站的 `base_url` 规范化后请求其 `/models` 路径，并发送 `Authorization: Bearer <OPENAI_API_KEY>`。工具不会自动轮询；刷新由用户触发，成功结果缓存在本地。

## 切换安全

“保存”只更新 `~/.codex-switch/profiles.toml`；“应用”才会修改 `~/.codex/config.toml` 和 `~/.codex/auth.json`。

每次应用都会先校验配置、生成备份，再原子替换目标文件。发生写入失败时会尝试回滚；下次启动也会检查未完成事务。工具只对它管理的相关字段做外部变更检测，出现冲突时应在界面中选择导入当前值、覆盖或取消，而不是继续盲写。

如果 Codex CLI 或 Codex/ChatGPT 桌面应用仍在运行，它可能已经把旧配置加载到内存，也可能在退出时再次写文件。工具会先给出风险提示，并在支持的平台上请求桌面应用正常退出和重新启动；无法可靠识别或关闭时，需要用户明确确认。不要在正在执行的重要 Codex 任务中强制切换。

备份只用于恢复 Codex 的 `config.toml` 和 `auth.json`，恢复操作同样会覆盖当前对应文件。删除一个工具配置不会自动修改当前已应用的 Codex 配置。

## 安装

发布产物按平台生成：

- macOS：`Codex Switch.app` 和 `.dmg`。
- Windows：NSIS `.exe` 安装器，默认安装到当前用户范围，不要求为所有用户安装。

在没有配置开发者签名证书时，产物是未签名的。macOS Gatekeeper 或 Windows SmartScreen 可能显示来源提示；请只运行自己构建或来自可信发布渠道的产物。

## 本地开发

需要 Rust 1.92 或更高版本，以及当前平台的 Rust 原生构建工具链。

```bash
cargo run
cargo test
cargo build --release
```

应用只读写当前用户家目录下的 `~/.codex` 和 `~/.codex-switch`。开发测试前建议先备份真实的 `~/.codex/config.toml` 与 `~/.codex/auth.json`。

## 打包

打包使用 `cargo-packager` 0.11.8：

```bash
cargo install cargo-packager --version 0.11.8 --locked
cargo packager --release
```

原生安装包应在对应目标系统上构建。配置中的 `formats = ["default"]` 在 macOS 上生成 `.app` 与 `.dmg`，在 Windows 上生成 NSIS 安装器。Windows 安装器使用 `currentUser` 模式。

## 许可

Codex Switch 使用 [MIT License](LICENSE)。Slint 和 Lucide 图标的许可及归属说明见 [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md)。应用顶部可访问的“关于”界面包含 Slint 的 `AboutSlint` 组件，用于满足所选 Slint royalty-free 许可的署名条件。
