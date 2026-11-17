# AgentDock

在手机和电脑上连接本机正在运行的 AI 编程助手会话（Kiro CLI / Claude Code），远程查看对话、继续对话、审批工具调用，并把对话中确认的待办自动收集到待办列表。

## 解决的问题

- 和 AI 说好"这个回头要做"，忙起来就忘了 → 对话里确认的待办自动进入 App 待办列表，能跳回来源对话
- 离开工位后 AI 卡在"是否允许执行命令"上等审批 → 在手机上直接查看完整命令并允许或拒绝
- 多个会话开在不同终端窗口里 → 在一个界面里同时连接最多 4 个会话

## 功能（规划）

| 功能 | 说明 |
|---|---|
| 会话发现 | 列出本机活跃的 Kiro 会话，显示工作目录、标题、所在 tmux 窗格 |
| 只读查看 | 不打断终端，直接查看会话历史 |
| 接管 / 交还 | 接管终端里的会话在 App 中继续对话，断开后可在终端用 `kiro-cli --resume-id` 续接 |
| 远程审批 | AI 调用工具时弹出审批卡片，显示完整参数，允许一次 / 总是允许 / 拒绝 |
| 待办收集 | AI 输出 `【待办候选】` → 用户确认 → AI 输出 `【待办已确认】` → 自动入库 |
| 多端 | Ubuntu、Android、Windows（Flutter 一套代码） |

## 架构

```
Ubuntu 开发机
 ├─ daemon（Rust 常驻服务）
 │   ├─ discovery    会话发现：锁文件 + 进程存活检查 + 父进程链 → tmux 窗格
 │   ├─ acp_host     通过 ACP（Agent Client Protocol）驱动 kiro-cli acp 进程
 │   ├─ session_log  读取会话 JSONL 文件
 │   ├─ todo         待办标记识别与存储
 │   └─ api          WebSocket 接口
 └─ kiro-cli acp 进程（由 daemon 启动，最多 4 个）
        ↕ WebSocket + JSON
app（Flutter：Linux / Android / Windows）
```

## 开发阶段

| 阶段 | 内容 | 状态 |
|---|---|---|
| S0 | ACP 前期验证（审批、历史加载、会话锁、接管交还） | 完成，见 [docs/S0-实测记录.md](docs/S0-实测记录.md) |
| S1 | daemon 骨架：会话发现、只读查看、WebSocket 接口 | 完成 |
| S2 | 接管与对话（S2 期间工具调用一律自动拒绝） | 完成 |
| S3 | 远程审批（审批卡片显示完整参数，先回复者生效，不回复就一直等待） | 完成 |
| S4 | 待办收集（扫描会话文件中的【待办已确认】标记，SQLite 存储，预留优先级/截止日期） | 完成 |
| S5 | Flutter Linux 版 App（会话 / 对话 / 审批 / 待办 四页，宽屏侧栏、窄屏底栏） | 完成 |
| 二期 | Claude Code 接入、Android 端、飞书登录与设备配对、内网/VPN 访问 | 未开始 |
| 三期 | Windows 端、外网中转 | 未开始 |

## 运行

```bash
# 1. daemon（只监听 127.0.0.1:7820）
cd daemon && cargo build --release && ./target/release/agentdock-daemon

# 2. App（Linux 桌面）
cd app && flutter build linux --release
./build/linux/x64/release/bundle/agentdock_app
# 连接其他地址：flutter run -d linux --dart-define=AGENTDOCK_URL=ws://host:port/ws

# 3. 命令行客户端（调试用）
cd daemon && ./target/release/dockctl sessions
```

测试：`cd daemon && cargo test`；`cd app && flutter test`；真实端到端（需 daemon 在运行，会消耗模型调用）：`cd app && flutter test integration_test/e2e_real_test.dart -d linux`。

## 目录结构

```
daemon/   Rust 常驻服务
app/      Flutter App
docs/     设计文档与实测记录
spike/    前期验证脚本（acp_probe.py）
```

## 开发环境

- Rust 1.96+
- Flutter 3.44+（Linux 桌面工具链）
- kiro-cli 2.13.0（已验证版本，会话文件格式无公开规范，升级后需复测）
- tmux

## 安全说明

远程审批等同于远程控制开发机执行命令：

- 一期 daemon 只监听 `127.0.0.1`，其他机器无法连接
- 二期开放内网访问时，必须加密传输，并经过飞书身份校验和一次性配对码绑定设备
- 审批记录写入 `~/.local/share/agentdock/audit.jsonl`（权限 0600，含被审批的完整命令）
- 没有任何自动批准：审批请求无人回复时 AI 一直等待
- 会话内容只在本机处理，不上传

## 许可

个人项目，暂未指定开源许可。
