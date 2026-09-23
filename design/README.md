# design/

Screen TUI 的设计文档。按下列顺序阅读。

| 文档 | 内容 | 状态 |
| --- | --- | --- |
| [`requirements.md`](./requirements.md) | **需求文档**。定位与非目标、功能需求（P0/P1/P2 分级 + 验收标准）、非功能需求、界面与按键规格、Screen 命令映射、配置数据模型、状态机、诊断检查项、版本兼容策略、技术选型、里程碑、决策记录 | v0.2，决策已确认（§14） |
| [`tech-design.md`](./tech-design.md) | **技术方案（Rust）**。ratatui + crossterm、无 async 事件循环、让出终端式连接（RAII 终端守护）、`-ls` 两级解析策略、预览实现、依赖清单、Docker 跨平台编译（cargo-zigbuild + musl）、CI/发布物、测试策略、风险清单 | v1.0 |
| [`development-plan.md`](./development-plan.md) | **开发计划**。M0–M3 任务分解（T 编号 + 依赖 + 验收标准）、横切约定（每任务一提交 / CI 门禁 / fixture 先行）、关键路径与服务器补测时点 | v1.0 |
| [`screen-capabilities.md`](./screen-capabilities.md) | GNU Screen 的能力边界：三层控制面（命令行 / `-X` 注入 / `-Q` 查询）、`-ls` 格式差异与解析方案、版本能力矩阵、本机实测记录、14 条陷阱、能力→需求映射 | 已完成 |
| [`reference-projects.md`](./reference-projects.md) | README 中 6 个参考项目的逐个分析与功能覆盖矩阵，提炼出共识功能、分歧议题与差异化机会 | 已完成 |

## 结论速览

**M0 已交付（2026-09-23）**：`caps` / `parse` / `stui ls` / `stui doctor` + Makefile / package.sh / CI。
32 个单测全绿，`fmt` + `clippy -D warnings` 双门禁通过。详见 `development-plan.md` §2.1。

**M0 期间实测推翻了三条原有判断**（都已回改进文档，不是「实现绕过设计」）：

| 原判断 | 实测 | 影响 |
| --- | --- | --- |
| `-q -ls` 退出码 9/10/11+「版本无关」 | 4.00.03 空列表返回 **8** | FR-19 验收 1 回改；退出码降级为快路径 |
| `-ls` 输出流未声明 | 走 **stdout**，行尾 `\r\n` | 解析前必须 strip `\r`，否则状态词全部匹配失败 |
| 构建镜像 `cargo-zigbuild:v0.19.8` | **该 tag 不存在**，GHCR 上不带 `v`，最高 0.17.1 | Makefile / CI 同步修正 |

**已确认的技术决策（2026-09-23）**：Rust 单二进制 + ratatui/crossterm + 无 async；连接走「让出终端 exec screen」；兼容 Screen 4.00.03；Linux（amd64/arm64 musl）用 Docker 交叉编译、macOS 本机编译；仅 GitHub Releases 二进制下载。详见 `tech-design.md`。

**Screen 能提供什么**

- 生命周期全覆盖：`-dmS` 建、`-ls` 枚举、`-r`/`-x`/`-d -r` 连接、`-X detach` 断开、`-X quit` 终止、`-wipe` 清理。
- 只读预览：`-X hardcopy [-h]`，**不 attach 就能拿到会话画面**。
- 读取类信息只有 `-Q` 能给（`windows`/`title`/`number`/`info`），而 `-Q` 在 4.6 之前不存在 —— 本机 macOS 自带的 4.00.03 实测即 `Unknown option -Q`。
- **Screen 完全不提供**：工作目录、运行命令、语义标签、收藏、历史。这些正是本工具的增量价值所在。

**参考项目的共识**

会话列表、连接后 detach 回到 TUI、连接前提示 `Ctrl-A D`、危险操作二次确认、新建时支持目录与启动命令 —— 六选三以上都做到了。其中「detach 后回到 TUI」是头号体验契约。

**还没人做好的**

1. 统一的版本兼容与能力探测（有人被 macOS 的 `-ls` 格式打脸后才修）。
2. 老版本（4.x）上的预览 —— 成本低、感知强。
3. 小屏的**信息分级**，而不只是布局切换（README 明确要求）。
4. 解析层的单元测试。

**最需要先拍板的一件事**：技术栈。本机实测的老版本 Screen + 服务器零依赖诉求，指向「Python 3 单文件 + 标准库 `curses`」；若要发行二进制则选 Rust。详见 `requirements.md` §12 与 §14。
