# CSwitch

在 Codex 官方登录和多个第三方 API 供应商之间切换。

## 使用

1. 从 Releases 下载对应系统的安装包，打开 CSwitch。
2. 点击官方登录，保存现有登录态，或在浏览器完成登录。
3. 点击右上角加号，选择添加 API 供应商或添加官方账号。API 填写名称、API URL 和 API Key；官方账号在浏览器完成 OAuth 登录。可以添加多个官方账号，点击对应账号卡片切换。
4. 点击供应商卡片切换。需要转换协议时，确认启用本地路由。
5. 点击刷新按钮更新该供应商的模型列表和数量；点击当前供应商可重新应用配置。
6. 在左上角开启常驻本地路由，或升级旧版路由配置后，重新打开 Codex。之后点击官方登录或供应商卡片切换线路，使用 HTTP/SSE 流式请求；开关旁的信息按钮可查看路由说明。
7. 使用常驻路由时保持 CSwitch 运行。关闭窗口后驻留托盘；托盘选择退出并停止路由会停止转发，重新打开 CSwitch 后恢复原端口。
8. 关闭常驻本地路由开关可恢复启用前的提供方、请求地址和传输设置，再重新打开 Codex。

## 切换时执行的操作

- 官方账号按用户和工作区独立保存，卡片显示账号名称及工作区编号；重复登录同一账号更新凭据。添加账号保留当前线路和当前登录文件，取消后返回原界面。
- 自动导入旧版官方快照及当前官方登录；切换官方账号前关闭 Codex 并保存最后一次令牌更新，只验证选中账号。常驻路由保持端口和配置，切换账号后重新打开 Codex。
- 账号列表只传递名称、工作区和状态，登录令牌保存在 `~/.codex/cswitch-profiles/official-accounts/`，每个账号一份文件，更新时原子替换。

- 启动时读取当前 `model_provider` 对应的提供方，支持自定义编号、带引号的名称和内联配置表，自动加入供应商列表；已有列表也会检查新出现的当前供应商。
- 自动导入从 `auth.json` 的 `OPENAI_API_KEY`、提供方原有的 `experimental_bearer_token`，或 `http_headers` 中的固定 `Authorization: Bearer` 读取密钥，按生效认证优先级选取。
- 导入结果保存到 CSwitch 供应商快照，保留原始配置副本；地址和密钥相同的记录复用，名称冲突时添加提供方编号区分。
- 使用环境变量认证、命令取令牌、AWS 签名或其他待确认认证方式时，显示具体原因，用户可在 API 配置中补全固定密钥。检测到系统凭据库配置时，提示先在 Codex 中选择文件认证后重新导入。
- 普通直连切换将选中供应商的密钥写入 `auth.json`，并清理生效提供方中会覆盖该密钥的旧认证字段。路由接管保存这些字段，临时使用官方认证，停用路由时恢复原值。
- 使用普通 API Key 直连切换时，关闭 Codex 桌面端，再读取最新的 `config.toml`、`auth.json`。检测到其他 Codex CLI 或后台服务仍在运行时，显示具体进程。
- 保存官方登录和各供应商的配置、认证快照。
- 以当前配置为基础更新第三方提供方：

```toml
model_provider = "custom"

[model_providers.custom]
name = "供应商名称"
base_url = "供应商地址"
wire_api = "responses"
requires_openai_auth = true
```

- 默认将第三方密钥保存到 `auth.json`：

```json
{
  "OPENAI_API_KEY": "供应商 API Key"
}
```

- 开启常驻本地路由时，备份配置并把请求地址设为固定回环地址。官方登录通过 `cswitch_local` 提供方接入代理，设置 `name = "OpenAI"`、`requires_openai_auth = true`、`wire_api = "responses"`、`supports_websockets = false`。已有第三方提供方保持原编号，更新其地址并设置 `supports_websockets = false`。
- 首次从官方提供方接管、升级旧版官方路由，以及完全停用时，关闭 Codex 并同步会话首行和 SQLite 的提供方。首次历史迁移耗时取决于会话文件总量。
- 路由模式保留官方 `auth.json`。代理向第三方发送该供应商的 API Key，切回官方时发送原官方认证。
- 之后切换线路时，原子更新 CSwitch 的出口选择。每个 HTTP 请求使用当时选定的线路，生成中的请求继续在原线路完成。
- 本地路由明确使用 HTTP/SSE，从首条请求开始进行流式输出；原生 Responses 路由透传模型参数、工具调用、compact 和模型接口。官方入口保留内置提供方的独立网页搜索能力标记。
- 官方出口透传 zstd 压缩请求；第三方出口先解压再转发 JSON，解压后请求体上限为 32 MiB。
- 第三方返回 401 时报告供应商密钥错误；官方返回 401 时交由 Codex 处理认证。
- 常驻代理随 CSwitch 退出而停止；启动 CSwitch 时重新绑定保存的端口，端口被占用时显示错误。
- 切换操作从点击开始加锁，取消官方登录立即收起进度框，后台结束后恢复按钮。
- 普通 API Key 直连切换检查 `sessions`、`archived_sessions` 和 SQLite，更新提供方不一致的任务记录。
- 备份配置、认证、会话首行和 SQLite，最多保留十份备份。失败时恢复本次修改，重启后继续处理未完成事务。
- 从 `/v1/models` 或 `/models` 读取模型编号，支持分页、去重和手动刷新，在供应商卡片显示模型数量。
- 原生 Responses 供应商直接连接；Chat Completions 和 Anthropic Messages 供应商通过本地路由转换文本和工具调用。路由支持并发请求和实时流式输出。
- 首次启用路由先启动代理，配置事务提交成功后清理旧代理。
- 启动失败时结束并回收新路由进程；逐个清理旧路由并汇总错误路径。
- 官方认证保存当前有效凭据及刷新后的令牌；从路由模式恢复 API Key 直连前，保存 Codex 最后写入的官方令牌。
- Windows 持有已核验的进程句柄等待退出，核对程序路径与创建时间，区分已经退出与真实权限错误。
- 操作失败时显示目录、阶段和原因；成功后的清理异常单独提示。
- 清理旧快照前检查配置和恢复备份的模型目录引用，保留仍被引用的文件。
- 写入或恢复配置、认证、设置前检查外部修改，发现冲突时保留用户新内容并报告路径。

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

## 本地开发

```bash
pnpm install --frozen-lockfile
pnpm tauri dev
```

检查：

```bash
pnpm build
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --all-features -- -D warnings
cargo build --manifest-path src-tauri/Cargo.toml
python scripts/run-process-tests.py
```
