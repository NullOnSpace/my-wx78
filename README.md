# my-wx78

基于 [qq-bot](https://github.com/NullOnSpace/silly-wx78) 库实现的自定义 QQ 机器人消息收发服务器，通过 Redis 作为消息中转桥梁，实现 QQ 与外部系统之间的双向消息传递。

## 功能

### 收取订阅消息

从 Redis 订阅频道 `REDIS_SOURCE_CHANNELS` 读取外部消息，调用 QQ Bot 直发消息接口将内容发送给指定用户（`QQ_OPEN_ID`）。

### 转发消息

监听 QQ 机器人通过 WebSocket 接收的用户消息，将消息内容序列化为 JSON 后发布到 Redis 的 `REDIS_RETWEET_CHANNEL` 频道，供外部系统消费。收到用户消息后会自动回复"收到"。

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
| `QQ_OPEN_ID` | 接收消息的 QQ 用户 OpenID，用于直发消息 |

### Redis 配置

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `REDIS_URL` | Redis 连接字符串，可包含密码、端口、数据库索引（如 `redis://:password@host:port/db`） | `redis://localhost:6379` |
| `REDIS_SOURCE_CHANNELS` | 订阅的频道列表，多个频道用逗号分隔 | `qqbot,qqbot2` |
| `REDIS_RETWEET_CHANNEL` | QQ 消息转发目标频道 | `qqbot_retweet` |

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

程序启动后会并行运行两个异步任务：

1. **WebSocket 监听** — 连接 QQ Bot 网关，接收用户消息并转发到 Redis，同时回复"收到"
2. **Redis 订阅** — 监听外部消息频道，收到消息后通过 QQ Bot 直发接口推送给用户

任一任务异常退出时程序终止。

## 转发消息格式

转发到 `REDIS_RETWEET_CHANNEL` 的消息为 JSON 格式：

```json
{
  "message_id": "消息ID",
  "author_id": "发送者ID",
  "author_openid": "发送者OpenID（可选）",
  "content": "消息内容",
  "timestamp": "时间戳"
}
```

## 项目结构

```
src/
  config.rs    — 环境变量配置加载
  redis.rs     — Redis 连接、发布与订阅
  handler.rs   — QQ 消息处理器（转发到 Redis + 回复"收到"）
  main.rs      — 主入口，并行启动双任务
```

## 部署

### 直接运行

确保 Redis 服务可用，配置好 `.env` 后：

```bash
cargo run --release
```

### 后台运行

使用 systemd 或 nohup 让程序在后台持续运行：

```bash
nohup ./my-wx78 &
```

或创建 systemd service 文件 `/etc/systemd/system/my-wx78.service`：

```ini
[Unit]
Description=my-wx78 QQ Bot Service
After=network.target

[Service]
Type=simple
WorkingDirectory=/path/to/my-wx78
ExecStart=/path/to/my-wx78/target/release/my-wx78
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl enable my-wx78
sudo systemctl start my-wx78
```