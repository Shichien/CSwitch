# CSwitch

CSwitch 用于在 Codex 官方登录和多个第三方 API 供应商之间切换。

## 功能

- 保存官方登录对应的 `auth.json` 和 `config.toml`，切回官方时按原内容恢复。
- 为每个第三方供应商分别保存名称、API URL、API Key、配置快照和模型列表。
- 添加或编辑供应商时探测 Responses、Chat Completions 和 Anthropic Messages 接口，并从 `/v1/models` 读取模型 ID。
- 在供应商卡片中显示从 `/v1/models` 读取到的模型数量。
- 切换第三方供应商时关闭正在运行的 Codex，并更新当前 `config.toml` 中的以下内容：

```toml
model_provider = "custom"

[model_providers.custom]
name = "供应商名称"
base_url = "供应商地址"
wire_api = "responses"
requires_openai_auth = true
```

- 以当前 `config.toml` 为基础保留用户设置，包括模型、思考强度、桌面设置、插件和 MCP 配置。
- 默认将第三方 API Key 写入 `auth.json`：

```json
{
  "OPENAI_API_KEY": "供应商 API Key"
}
```

- 可选保持官方 ChatGPT 登录：切换第三方时保留 `auth.json`，并把 API Key 写入 `[model_providers.custom]` 的 `experimental_bearer_token`。默认关闭。
- 将 `sessions`、`archived_sessions` 和 `state_5.sqlite` 中的任务提供方同步为当前 Codex 提供方，使已有任务在切换后继续显示。
- 对 Chat Completions 和 Anthropic Messages 供应商启动本地协议转换，并向 Codex 提供 Responses 接口。
- 本地网关会丢弃没有 `encrypted_content` 的 reasoning，并去掉非 `rs_` 前缀的 ID，避免第三方接口返回非法 reasoning ID。
- 切换供应商、官方登录和保存配置时显示分步进度。
- 关闭主窗口时最小化到系统托盘；托盘菜单可打开窗口、切换供应商或退出。
- 每次切换前备份配置、认证、任务记录和 SQLite；切换失败时恢复本次修改。

## 使用

1. 从 [Releases](../../releases) 下载对应系统的安装包并安装 CSwitch。
2. 打开 CSwitch。已有有效官方登录时，点击官方登录即可保存并使用；没有有效登录时，按浏览器页面完成登录。
3. 点击右上角加号，填写供应商名称、API URL 和 API Key。
4. 保存供应商。CSwitch 会检测接口并读取模型数量。
5. 如需在第三方供应商下保持 ChatGPT 登录态，先打开「保持官方登录」。
6. 点击供应商卡片完成切换。界面会显示当前步骤。供应商需要协议转换时，先按界面提示启用本地路由。
7. 点击官方登录卡片即可恢复官方配置和登录状态。关闭窗口后程序继续在托盘运行。

## 数据位置

```text
~/.codex/auth.json
~/.codex/config.toml
~/.codex/cswitch-profiles/
~/.codex/cswitch-backups/
~/.codex/sessions/
~/.codex/archived_sessions/
~/.codex/state_5.sqlite
~/.codex/sqlite/state_5.sqlite
```
