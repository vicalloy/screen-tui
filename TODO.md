# TODO — screen-tui

## M1 P0 核心闭环（已完成，实机验收待 T0.6）

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
- [x] T1.4 新建会话（2026-09-23）
- [x] T1.5 连接闭环·编码（2026-09-23）/ ⚠️ **实机验收等 T0.6**（必须 T1.5 验收前完成）
- [x] M1 出口自查·状态机部分（2026-09-23）：`m1_exit_criterion_full_walk` 覆盖
      「看 → 选 → 进 → 出」+ FR-03 返回契约（替身注入，非实机）；
      40 列渲染覆盖见 `ui::list` / `ui::layout` 的 TestBackend 断言。
      **实机 40 列走查待 T0.6 后补做。**

## T0.6 之后的实机验收清单（M1 收尾）

1. `screen -r <name>` 在真实多会话下的歧义表现，验证 `unambiguous_target` 回退（1.5e）。
2. `-x` 共享与 `-d -r` 接管的实际效果（确认无需 `-D -r`，FR-03 表）。
3. dead 会话的拒连文案与 `-wipe` 行为（T2.4 依赖）。
4. detach（Ctrl-A D）后 TUI 是否完整恢复焦点/尺寸（1.5d 的实机部分）。
5. `$STY` 非空嵌套场景的 screen 真实反应。
6. 手机尺寸（40×20、50×12）下的真实 40 列走查：看 → 选 → 进 → 出。

---

# TODO — M2 P1 增强

- 依据：`design/development-plan.md` §4、`design/requirements.md` §4.2（FR-10~24）、`design/tech-design.md` §3.3/§3.4/§3.5
- 出口 = requirements.md §13「整体验收」4 条 + M2 出口：**危险操作均二次确认；预览降级路径可用**
- 横切约定沿用：每任务一提交、CI 三门禁、替身注入可测、禁止 `stuff`
- M2 开工地基：M1 全量（89 tests，fmt+clippy 绿）；`config.rs` 仅有目录解析与可写探针；`TempFile`（0600+RAII）已在

---

## 任务分解

### T2.1 配置层（T2.6/T2.7 的地基）

| # | 子任务 | 验收 |
|---|---|---|
| 2.1a | `Config` 结构（§8.2：`version` / `ui{layout,narrow_cols,wide_cols,refresh_ms,icons}` / `defaults{use_utf8,prefer_256color,attach_after_create,escape_prefix}` / `dirs` / `sessions`）+ `Default` | serde 往返单测；默认值即当前 M1 行为 |
| 2.1b | 原子写：`config.json.tmp` → fsync → rename；目录 0700、文件 0600 | 单测断言权限与「无 tmp 残留」 |
| 2.1c | 加载降级：JSON 损坏 → 备份 `.bak` → 默认值启动并提示；`version` 高于已知 → **只读** + 警告，不覆写（§8.3） | 两条路径单测 |
| 2.1d | 接入 App：`refresh_interval` / 布局阈值 / `escape_prefix` 覆盖来自配置；`run()` 装配 | App 单测：配置值生效 |

### T2.2 元数据探测 `screen::probe`

| # | 子任务 | 验收 |
|---|---|---|
| 2.2a | `probe::session_meta(pid) -> Option<Meta{cwd, command}>`：Linux 走 `/proc/<pid>/cwd` readlink + cmdline（首个非 screen 子进程）；macOS 走 `ps -o command` + `lsof -a -d cwd -Fn` | 输出解析纯函数单测（fixture 化 ps/lsof 样本） |
| 2.2b | 任何一步取不到 → `None`，UI 隐藏字段不猜测（C-5）；探测失败不影响列表主流程 | `None` 路径单测；详情渲染无该字段 |
| 2.2c | 接入：宽屏/详情面板显示 cwd + command；`App` 带 meta 缓存（按 pid，随 refresh 失效） | TestBackend 断言字段出现/隐藏两态 |

### T2.4 会话操作（危险操作 + FR-18）

| # | 子任务 | 验收 |
|---|---|---|
| 2.4a | `cmd::remote_detach`（`-S <full> -X detach`）、`cmd::kill`（`-X quit`）、`cmd::rename`（`-X sessionname <new>`，复用 `validate_name`）、`cmd::wipe`（`screen -wipe`）——参数拼装纯函数 + 替身测试 | 替身断言参数精确、退出码透传 |
| 2.4b | `Mode::Confirm(ConfirmAction)`：K/W/D 通用确认框，**默认焦点在取消**；框内显示会话名 + 探测到的运行命令（FR-13 验收 2）；`←/→/Tab` 切焦点、`Enter` 执行焦点项、`y` 显式确认、`Esc/n` 取消 | 状态机单测：初始焦点 = 取消；各键转换 |
| 2.4c | 操作前重新枚举校验（NFR-08）：目标消失/状态不匹配 → 明确提示 + 刷新，不执行 | 替身测试：消失 → 不发命令 |
| 2.4d | 键位接线：`D` 仅 Attached/Multi、`K` 非死会话、`r` 重命名（输入模式）、`W` dead 清理（确认后 `-wipe`） | 各键入口校验单测 |
| 2.4e | FR-18：探测 `~/.screenrc` / `$SCREENRC` 的 `escape` 行 → 实际前缀；detach 提示按实际前缀输出，探测不到回退默认 + 注记 | 解析单测（`escape ^Aa` / `escape x x` 两形态） |

### T2.5 过滤 + 数字直连 + 详情增强

| # | 子任务 | 验收 |
|---|---|---|
| 2.5a | 数字键 `1`–`9` 直连对应序号（§6.4 核心键，M1 缺项补齐）；复用 `start_connect` 全套重校验 | 单测：数字 → 对应会话的连接请求 |
| 2.5b | `Mode::Filter`：`/` 进入、输入即筛（name/pid/command/cwd）、`Esc` 清空、页脚显示过滤词与命中数；refresh 不重置过滤、不丢选中 | 状态机 + TestBackend 断言 |
| 2.5c | 详情增强：`-Q windows` 可用（`caps.query == Yes`）时显示窗口数；`Unknown/No` 时**整行隐藏**（FR-17 验收）；结果按 pid 缓存，随 refresh 失效 | 替身注入 `-Q` 输出单测 |
| 2.5d | 帮助弹层更新：n / Enter / 1-9 / x / p / i / D / K / r / W / / / R / ? / q 全量按键表 | 渲染断言 |

### T2.3 预览（hardcopy 快照，FR-15）

| # | 子任务 | 验收 |
|---|---|---|
| 2.3a | `screen::preview(full) -> Result<String>`：`TempFile`（0600+RAII）+ `screen -S <full> -X hardcopy <path>`；尾部空白裁剪、长行按宽度裁剪；每次重抓不缓存 | 替身注入 hardcopy 输出；裁剪纯函数单测 |
| 2.3b | `Mode::Preview` 弹层：标题含会话名 + 抓取时间；`p` 打开；**失败明确显示不可用原因，绝不显示上一次内容**（FR-15 验收 3） | 三路径（成功/失败/空输出）单测 |
| 2.3c | 宽屏右栏 Preview 常驻（Detail 下方），随选中项更新；窄屏仅 `p` 弹层 | TestBackend：宽屏有 / 窄屏无 |
| 2.3d | 降级：`caps.hardcopy != Yes` 时 `p` 直接给「预览不可用 + 原因」；dead 会话拒绝预览 | 替身单测 |

### T2.6 元数据持久化（FR-24）

| # | 子任务 | 验收 |
|---|---|---|
| 2.6a | `create` 成功后写 `sessions{name, managed:true, command, cwd}`；连接过（attach）的会话入库为 unmanaged 记录（仅含 last_seen） | 单测：两来源记录字段正确 |
| 2.6b | 别名/描述编辑：详情弹层 `a` 别名、`t` 描述（单行输入）；列表宽/中屏显示别名 | 状态机 + 渲染单测 |
| 2.6c | 重启：managed 会话 dead 或消失时可 `s` 用记录的 command+cwd 重建；unmanaged 明确禁用（提示原因） | 单测：managed 重启参数正确；unmanaged 拒绝 |
| 2.6d | 会话消失元数据**保留**；手动清理入口：列表 `C` 确认后删除已消失会话的全部元数据（GC 策略=手动，§14） | 单测：消失后仍在；清理后消失 |

### T2.7 收藏目录（FR-23）

| # | 子任务 | 验收 |
|---|---|---|
| 2.7a | `dirs{path,last_used}`：新建/连接（探测到 cwd）时自动入库；按 last_used 降序；上限可配默认 10，入满淘汰最旧 | 单测：排序、去重、上限淘汰 |
| 2.7b | 新建向导目录步：顶部列出最近目录，`1`–`9` 直选；手输路径不受影响 | 状态机 + 渲染单测 |

---

## M2 依赖与顺序

```
T2.1 配置 ──┬── T2.6 元数据 ── T2.7 收藏
            └── T2.4 会话操作 ── T2.3 预览（共用确认/重校验基建）
T2.2 probe ── T2.5 过滤/详情
```

实现顺序（已按此完成）：T2.1 → T2.2 → T2.4 → T2.5 → T2.3 → T2.6 → T2.7。

## 状态（2026-09-23 编码完成，143 tests / fmt / clippy 全绿）

- [x] T2.1 配置层（commit `a243d1b`）：原子写 / `.bak` 降级 / 高版本只读 / 阈值配置化
- [x] T2.2 元数据探测（commit `0b1c1b0`）：`/proc` 造假树单测 + ps/lsof 解析 fixture
- [x] T2.4 会话操作（commit `a93f894`）：确认框默认取消 / 执行前重校验 / FR-18 前缀探测
- [x] T2.5 过滤 + 数字直连（commit `6adbdf2`）：输入即筛 / `x` 显式共享 / `-Q` 窗口数隐藏语义
- [x] T2.3 预览（commit `3030a9c`）：0600+RAII / 空快照=不可用（歧义时不给空预览）
- [x] T2.6 元数据持久化（commit `c889881`）：managed/unmanaged / `a`·`t` 编辑 / `s` 重启 / `X` 清理
- [x] T2.7 收藏目录（commit `83fec98`）：去重置顶淘汰 / 向导数字直选 / 连接 cwd 入库

## 实现口径的三个偏离（有意为之，待实机复核）

1. **宽屏预览「随选中项更新」**：按选中项变化触发一次抓取 + 手动 `p`，不做 3 秒轮询
   （每轮询周期对会话做 hardcopy 落盘过于扰人；实机验证后如需可加自动跟随开关）。
2. **连接 cwd 入库用缓存窥视（`peek`）**：不为记目录额外起一次 `lsof`；
   取不到就不记（C-5），该会话此前被选中过时才自然有缓存。
3. **空快照（0 字节）按「预览不可用」处理**：无法区分「空窗口」与「没写盘」，
   按诚实口径拒绝渲染歧义内容（T0.6 实测后可再校准）。

## M2 验收边界

- `-X detach/quit/sessionname`、`-wipe`、`hardcopy` 的真实语义仍受 T0.6 制约：本机只能验证「参数拼装 + 替身注入 + 降级路径」；实机走查在 T0.6 补测后统一做。
- `kill -9` 后终端不报废的验收已随 T1.1 覆盖口径执行。

## T0.6 之后的实机验收清单（M2 补充）

1. `-X detach` / `-X quit` / `-X sessionname` 在真实会话上的效果与退出码（T2.4）。
2. `-wipe` 清理 dead 会话的实际行为与输出（T2.4）。
3. hardcopy 快照内容与空窗口时的字节语义（T2.3，校准上面偏离 3）。
4. `-Q windows` 输出格式与 `parse_window_count` 计数正确性（T2.5）。
5. `s` 重启：dead managed 会话重建后旧 dead socket 的处理（是否需要先 wipe）。
6. FR-18：`.screenrc` 含 `escape` 行时 detach 提示前缀是否符合用户实际配置。
7. 手机尺寸下 M2 全部新键位（1-9 / x / p / D / K / r / W / s / X / /）的可用性走查。


