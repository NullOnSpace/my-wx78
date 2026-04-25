# 自定义QQ机器人收发服务器

基于依赖中的qq-bot库实现QQ机器人的消息收发

## 基础配置
使用`.env`进行基础配置

用于qq-bot库的配置:

- `QQ_BOT_APP_ID`: QQ机器人的应用ID
- `QQ_BOT_CLIENT_SECRET`: QQ机器人的应用密钥
- `QQ_OPEN_ID`: QQ用户的OpenID，用于直发消息

用于接收非qq的外部消息中转Redis:

- `REDIS_URL`: redis数据库的连接字符串

Redis相关配置应提供默认值

Redis中订阅的频道:

- `REDIS_SOURCE_CHANNELS`: 订阅的频道列表，多个频道用逗号分隔，例如: `qqbot,qqbot2`
- `REDIS_RETWEET_CHANNEL`: 转发的频道，例如: `qqbot_retweet`

## 功能

一个能长期运行的后台项目，使用异步完成网络IO，具体异步的结构由你决定。主要实现以下功能:

### 收取订阅消息

- 从订阅的频道`REDIS_SOURCE_CHANNELS`中读取消息，调用发送消息接口发送给用户

### 转发消息

- 一个监听QQ机器人接收的消息，转发到`REDIS_RETWEET_CHANNEL`。


