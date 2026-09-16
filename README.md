# CSwitch

在 Codex 官方登录和多个第三方 API 供应商之间切换。

## 使用

1. 从 Releases 下载对应系统的安装包，打开 CSwitch。
2. 点击官方登录，保存现有登录态，或在浏览器完成登录。
3. 点击右上角加号，填写供应商名称、API URL、API Key 并保存。
4. 点击供应商卡片切换。需要转换协议时，确认启用本地路由。
5. 点击刷新按钮更新该供应商的模型列表和数量；点击当前供应商可重新应用配置。
6. 点击官方登录切回保存的官方配置。关闭窗口后驻留托盘，托盘菜单提供退出入口。

## 切换时执行的操作

- 关闭 Codex 桌面端，再读取最新的 `config.toml`、`auth.json`。检测到其他 Codex CLI 或后台服务仍在运行时，显示具体进程。
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

- 开启保留官方登录后，保留官方 `auth.json`，在当前供应商的 `experimental_bearer_token` 中设置第三方密钥。关闭该开关时移除这个字段，并把密钥写回 `auth.json`。
- 检查 `sessions`、`archived_sessions` 和 SQLite，只更新提供方不一致的任务记录。
- 备份配置、认证、会话首行和 SQLite，最多保留十份备份。失败时恢复本次修改，重启后继续处理未完成事务。
- 从 `/v1/models` 或 `/models` 读取模型编号，支持分页、去重和手动刷新，在供应商卡片显示模型数量。
- 原生 Responses 供应商直接连接；Chat Completions 和 Anthropic Messages 供应商通过本地路由转换文本和工具调用。路由支持并发请求和实时流式输出。
- 切换本地路由时先启动新路由，提交成功后停止旧路由。
- 启动失败时结束并回收新路由进程；逐个清理旧路由并汇总错误路径。
- 官方登录检查服务端认证状态，保存刷新后的令牌。取消登录或关闭窗口会结束登录等待。
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
