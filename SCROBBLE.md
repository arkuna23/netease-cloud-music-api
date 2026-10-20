# 真实播放上报（NCBL v3）

`MusicApi::scrobble_context(uid)` 冻结登录上下文；`MusicApi::scrobble(&context, &event)` 上传单个真实播放事件。调用方负责计时、账号隔离、事件顺序和发送策略。`Start` 对应 `_plv`，`End` 对应 `_pld`，`played` 与媒体 `duration` 分别使用 `Duration` 表达。`bitrate` 使用 bit/s，协议边界转换为 kbit/s。

## 协议与边界

- 来源：<https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced/tree/19c7dad6db7d660954c7a615a686ff0acb030f91> 的 `module/scrobble_v1.js`、`util/ncbl.js`（MIT）。协议背景：issue #167。
- 实现核对：2026-10-20；工作基线 `29cb56877e29ff350d5b1f55340602a187be618f`。
- 编码：NCBL v3，小端头部，专用 256-bit RSA 模幂包装，ChaCha20，gzip，每帧最多 32768 字节，multipart 独立上传。
- 端点：`https://clientlog3.music.163.com/api/clientlog/encrypt/upload?multiupload=true`，固定 PC 身份。请求复用代理，15 秒超时，采用固定目标且关闭重定向，认证字段使用白名单和冻结快照。
- 成功标准：HTTP 成功且响应 `code=200`、`data.successfiles` 包含本次文件名。`ScrobbleReceipt` 表示文件接收确认；官方统计由账号实测验收。
- 上报真实来源；未知来源保留为空。界面访问轨迹 `_addrefer`、`_multirefers` 采用空值。结束事件使用参考协议的 `interrupt`。
- 一次调用对应一次上传；传输异常可能发生在服务端接收之后，调用方应保留结果未知状态。

## 本机手工验证

使用权限仅自己可读的文件保存账号 Cookie 请求头内容，如 `MUSIC_U=...; __csrf=...`。文件路径作为参数传入，认证内容仅用于本机请求。请保持账号凭据私密。

```sh
cargo run --example scrobble -- /private/path/cookies.txt YOUR_UID SONG_ID TOTAL_SECONDS
```

代理可通过 `SCROBBLE_PROXY` 环境变量指定。示例应与真实播放器配合：实际开始播放后按 Enter，持续正常播放，实际结束时再次按 Enter。更复杂的暂停、seek 等场景请通过 GTK 客户端的播放记录器验证。

分别在上报前、结束后、15 分钟和次日检查官方端，并填写：

| 观察时间 | 最近播放 | 听歌排行 | 累计听歌时长 | 官方端截图/备注 |
|---|---|---|---|---|
| 播放前 | | | | |
| 结束后 | | | | |
| 15 分钟 | | | | |
| 次日 | | | | |

交付状态：实现验收见检查结果；**入账待用户实测验证**。上传确认和三类官方统计分别记录。

## 兼容性证据

单元测试中的固定 RSA 包装值与元数据密文由上述固定 revision 的 JavaScript 实现生成；解码测试核对 NCBL 布局、gzip 和正文。针对性检查：`cargo test scrobble --lib`、`cargo check --example scrobble`。
