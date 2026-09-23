# TODO — M1 P0 核心闭环

- 依据：`design/development-plan.md` §3、`design/requirements.md` §4.1（FR-01/02/03/04/05）、`design/tech-design.md` §2/§3.1
- 出口 = requirements.md「整体验收」第 1、3 条：**纯键盘 40 列全流程（看 → 选 → 进 → 出）+ detach 必回列表**
- 横切约定（development-plan.md §6）：每任务一提交、CI 三门禁、`STUI_SCREEN` 注入可测、**禁止任何 `stuff` 注入路径**
- M1 开工时的既有地基：`screen::{caps,parse,cmd}`（32 测试全绿）、`util::width`、`stui ls`/`doctor`；依赖已锁定（ratatui 0.29 / crossterm 0.28 / libc 0.2）

---

## 任务分解

### T1.1 终端守护 + 事件循环骨架

| # | 子任务 | 验收 |
|---|---|---|
| 1.1a | `ui::TuiGuard`（RAII）：raw mode / alternate screen / 光标隐藏，`Drop` 无条件还原；`suspend()`/`resume()` 供连接闭环复用（tech-design §3.1） | 正常退出后终端状态完好（stty 可查证） |
| 1.1b | panic hook（先还原终端再走默认 hook）+ SIGINT/SIGTERM handler（libc 注册，置标志位走正常退出路径） | panic / Ctrl-C / kill 后终端不报废 |
| 1.1c | `app.rs` 状态机：`Mode` 枚举 + `on_key` 分发 + `next_tick_in`/`tick_due` + `refresh()`（复用 `parse::enumerate`，退出码只作快路径） | `Mode` 转换纯逻辑单测，不依赖渲染 |
| 1.1d | cli 接线：`stui`（无子命令）→ TUI 主循环；`Event::Key` 只响应 `KeyEventKind::Press`；**不启用键盘增强协议** | `q` 退出、事件循环单线程无 async |

**已知边界**：`kill -9` 不可捕获（OS 层面无回调），任何程序都无法在其路径上还原终端 —— 诚实处理：RAII 覆盖所有可捕获路径（正常/q/panic/SIGINT/SIGTERM/SIGHUP），`kill -9` 残留的 raw mode 由文档给出 `reset` 恢复说明。验收按此口径执行。

### T1.2 列表视图（fixture 先行）

| # | 子任务 | 验收 |
|---|---|---|
| 1.2a | fixture：CJK / emoji / 超长会话名 `-ls` 样本（含 attached+日期组合） | 解析通过（T0.3 解析器已守门，此为渲染层样本） |
| 1.2b | 状态图标映射（§6.3：○ detached / ◆ attached / ◈ multi / ✕ dead / ? unreachable，单字宽非 emoji）+ `ui::theme` 样式常量 | 图标与 `Status` 一一对应，未知状态显示 `?` 不猜 |
| 1.2c | `ui::list` 渲染：序号 + 图标 + 名字 +（可选列），`util::width` 全链路钳制；j/k/↑/↓ 移动；无会话空状态 + 「按 n 新建」引导（FR-01 验收 1）；dead 行提示 `W`（M2 实现，M1 仅提示） | **ratatui TestBackend** 断言：CJK/emoji 名不错行；超宽先截名字、绝不丢序号与状态（FR-01 验收 3） |

### T1.3 双布局 + 小屏信息分级

| # | 子任务 | 验收 |
|---|---|---|
| 1.3a | 布局档位：wide ≥100 / mid 72–99 / narrow <72 / tiny <50 或 高<15；阈值常量化 | 档位判定纯函数单测 |
| 1.3b | 信息分级（FR-05 表）：narrow 起隐藏 PID/时间/目录、状态只留图标、页脚极简；wide 双栏（列表 + 详情区） | TestBackend 断言 40×20 首屏 ≥5 条会话；各档位无溢出 |
| 1.3c | 隐藏信息的按键入口：极简 `i` 详情弹层（M1 只展示列表已有字段；`-Q` 窗口数等增强留 T2.5） | FR-05 验收 2「隐藏必有入口」在 M1 成立 |
| 1.3d | resize 自动重排（crossterm Resize 事件 → 下次 draw 生效）+ 手动锁定布局暂不做（T2.1 配置项） | TestBackend 模拟 40×20 → 120×40 两次渲染，布局随宽度变化 |

### T1.4 新建会话

| # | 子任务 | 验收 |
|---|---|---|
| 1.4a | `NewSession(NewDraft)` 三步向导（名 → 目录 → 命令），默认值自动生成 `目录名-MMDD-HHMM`，回车即接受（FR-02 验收 1） | 向导状态机单测（纯逻辑） |
| 1.4b | 名字校验：非法字符 / 超长即时报错；重名**允许创建**但提示「将以 `<pid>.<name>` 寻址」（FR-02 验收 2） | 校验规则单测 |
| 1.4c | `screen::cmd` 增加 `create(name, dir, command)`：`screen -U -dmS <name> <command>`，创建前 `cd` 到目标目录（子进程 cwd）实现「显式设置工作目录」；`screen-256color` terminfo 可用时为子进程设置 `TERM` | **`STUI_SCREEN` 替身测试**创建参数拼装正确；不真创建会话 |
| 1.4d | 错误路径：screen 缺失 / 目录不可写 / 创建失败 → 可行动报错，不静默；成功后回列表并选中新会话（FR-02 验收 4/5） | 替身注入失败退出码走通报错路径 |

### T1.5 连接闭环（编码可先行，**实机验收依赖 T0.6**）

| # | 子任务 | 验收 |
|---|---|---|
| 1.5a | 连接前重校验：重新 `-ls` 确认会话仍在且状态匹配（NFR-08）；dead/unreachable 拒连（FR-03 表） | 替身测试：会话消失 → 明确提示 + 刷新，不卡死（FR-03 验收 3） |
| 1.5b | attached 冲突选择框：`1` 共享 `-x` / `2` 接管 `-d -r`（**绝不用 `-D -r`**）/ `Esc` 取消；multi 额外提示尺寸风险 | 选择框状态机单测 |
| 1.5c | detach 提示：连接前打印 `Ctrl-A D`（M1 用默认前缀 + 「若改过前缀请用你的前缀 + d」注记；真实 `.screenrc` 探测属 FR-18/M2） | 提示文本单测 |
| 1.5d | 前台执行：`TuiGuard::suspend()` → spawn `screen -r/-x <full>` 继承 stdio 阻塞等待 → `resume()` + 强制重绘 + `refresh()`。**spawn 而非 exec**（tech-design §3.1 契约要求 detach 后能回到 TUI，exec 会替换进程无法返回） | 替身测试：子进程退出码任意 → 必回列表（FR-03 验收 2 头号契约） |
| 1.5e | 歧义名回退：`screen -r <name>` 被拒（不唯一）时自动用 `<pid>.<name>` 全名重试（FR-03 验收 4）；`$STY` 非空启动时警告一次（FR-03 验收 5） | 替身测试两条路径 |

**T1.5 验收边界**：`-r`/`-x`/`-d -r` 的真实语义、dead 呈现等 9 项仍待 T0.6 真实服务器补测（development-plan §7 明确：不阻塞编码，但**必须在 T1.5 验收前完成**）。本机只能验证到「替身注入 + 事件流正确」这一层。

---

## 依赖与顺序

```
T1.1 守护/骨架 ── T1.2 列表 ──┬── T1.3 布局
                              ├── T1.4 新建
                              └── T1.5 连接闭环（最后）
```

## 状态

- [x] T1.1 终端守护 + 事件循环骨架（2026-09-23）
- [x] T1.2 列表视图（2026-09-23）
- [x] T1.3 双布局（2026-09-23）
- [ ] T1.4 新建会话
- [ ] T1.5 连接闭环（编码）/ ⏸ 实机验收等 T0.6
- [ ] M1 出口自查：`stui` 在 40 列 TestBackend 下走通「看 → 选 → 进 → 出」+ FR-03 返回契约
