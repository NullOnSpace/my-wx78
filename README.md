# my-wx78

push-architecture 的 **D 层 QQ 连接器**：出向以 Redis Streams consumer group 消费 `stream:rendered:qq`（render-hub 预渲染信封）发送 QQ 消息并把回执 XADD 到 `stream:receipts`；入向把 QQ WebSocket 私聊消息构造为 canonical CHAT（push.v1）发布到 `stream:msgs:in`。设计依据 [push-architecture/DESIGN.md](../push-architecture/DESIGN.md) §5.7。

## 功能

### 出向：stream:rendered:qq → QQ 发送

consumer group `my-wx78` 消费 `REDIS_RENDERED_STREAM`（默认 `stream:rendered:qq`），解码 proto 编码的 `RenderEnvelope`，按 payload 链序尝试（成功即停）：

1. `QqMarkdown`（msg_type=2 原生 markdown，实测私聊可用）→ `send_markdown`
2. `Text` → 文本直发

- 每次平台发送经 dispatch-core 重试（指数退避 + full jitter；网络/超时类错误重试，400/权限类不重试）与令牌桶限流（`QQ_RATE_LIMIT_PER_MIN`，默认 30/min）。
- 每个信封发送后把 proto 编码的 `SendReceipt` XADD 到 `REDIS_RECEIPTS_STREAM`（默认 `stream:receipts`）：身份字段从信封原样回显，携带 ok/error/attempts/平台 message_id。
- 信封校验失败（channel 非 QQ / target 为空）只 WARN 丢弃（XACK，无回执）。
- 投递目标以 `RenderEnvelope.target_id` 为准；`QQ_OPEN_ID` 仅作入向回复兜底（见下）。

### 入向：QQ WS → canonical CHAT → stream:msgs:in

监听 QQ 机器人 WebSocket 收到的用户私聊消息，构造 push.v1 `CanonicalMessage`（kind=CHAT，`source`=IM/`qq-c2c`，`dedup_key`=`qq-c2c:{平台消息ID}`，domain.chat 携带作者/内容/时间）后 XADD 到 `REDIS_MSGS_STREAM`（默认 `stream:msgs:in`）供 push-core 路由；收到消息后自动回复"收到"。

## 配置

通过 `.env` 文件进行配置。复制模板并填入实际值：

```bash
cp .env.example .env
```

### QQ Bot 配置（必填）

| 变量 | 说明 |
|------|------|
| `QQ_BOT_APP_ID` | QQ 机器人的应用 ID |
| `QQ_BOT_CLIENT_SECRET` | QQ 机器人的应用密钥 |
| `QQ_OPEN_ID` | 入向回复兜底 OpenID（入向事件缺 `user_openid` 时用）；**不是**出向投递目标 |

### Redis / 出向配置

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `REDIS_URL` | Redis 连接字符串，可包含密码、端口、数据库索引（如 `redis://:password@host:port/db`） | `redis://localhost:6379` |
| `REDIS_MSGS_STREAM` | 入向 canonical CHAT 的 XADD 目标 stream（push-core 消费） | `stream:msgs:in` |
| `REDIS_RENDERED_STREAM` | 出向消费的渲染信封 stream（render-hub C 层产出） | `stream:rendered:qq` |
| `REDIS_RECEIPTS_STREAM` | 发送回执的 XADD 目标 stream（push-core 消费） | `stream:receipts` |
| `QQ_RATE_LIMIT_PER_MIN` | QQ 主动消息限流（个/分钟） | `30` |
| `RETRY_MAX_ATTEMPTS` | 单次发送最大尝试次数（含首次） | `3` |

### 日志

通过 `RUST_LOG` 环境变量控制日志级别，默认为 `my_wx78=info`：

```bash
RUST_LOG=my_wx78=debug cargo run
```

## 构建

```bash
cargo build --release
```

## 运行

```bash
cargo run --release
```

程序并行运行两个任务（JoinSet + CancellationToken，优雅关机互不影响）：

1. **入向 WebSocket** — 连接 QQ Bot 网关，接收用户消息构造 canonical CHAT 发布到 `stream:msgs:in`，同时回复"收到"
2. **出向连接器** — 消费 `stream:rendered:qq`，按 payload 链发送 QQ 消息并把回执 XADD 到 `stream:receipts`

## 入向 canonical CHAT 字段约定

`CanonicalMessage`（push.v1 proto，见 canonical-msg 仓）：

```text
id            = ULID（A 层生成，全系统追踪主键）
source        = { kind: SOURCE_KIND_IM, id: "qq-c2c", display_name: "QQ 私聊" }
dedup_key     = "qq-c2c:{平台消息ID}"
kind          = MSG_KIND_CHAT
title         = 空（聊天无标题）
body_markdown = body_text = content 原文
links/tags    = 空
published_at  = received_at = QQ timestamp 解析结果（解析失败用当前时间）
domain.chat   = { platform_message_id, author_id, author_openid, content, received_at, chat_id: "" }
```

## 项目结构

```
src/
  config.rs    — 环境变量配置加载
  redis.rs     — Redis 连接与入向 canonical CHAT 发布（XADD stream:msgs:in）
  handler.rs   — QQ 消息处理器（canonical 构造 + 发布 + 回复"收到"）
  outbound.rs  — 出向连接器（stream:rendered:qq 消费 → 重试/限流发送 → stream:receipts）
  main.rs      — 主入口，双任务并行 + 优雅关机
```

## 测试

```bash
cargo test
# 端到端回路测试（需本地 Redis，发布信封 → 断言回执）：
MY_WX78_TEST_REDIS=1 cargo test loop_publishes
```

## 部署

二进制部署到 `/opt/push/bin/my-wx78`，以 user service `push-my-wx78.service` 运行（旧 `my-wx78.service` 已退役）。配置见上表。
