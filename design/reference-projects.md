# 参考项目分析

> 来源：README.md 中列出的 6 个仓库，逐一读取其 README 与仓库结构后整理。
> 用途：找出「必须有的共识功能」与「还没人做好的空白」，作为需求取舍的依据。

## 1. 总览

| # | 项目 | 语言 / 技术栈 | 定位一句话 | 最值得借鉴 | 明显短板 |
| --- | --- | --- | --- | --- | --- |
| 1 | [spv](https://github.com/bezoodaog/spv) | Go + Bubble Tea | 双栏会话监视器，带实时资源占用 | 向导式新建、多主题、footer 常驻快捷键 | 依赖 Go 工具链；Linux-only 的 autostart；会在启动时请求 GitHub 拉动态标题 |
| 2 | [sm](https://github.com/m2kar/sm) | TypeScript + Ink(React) + Node 24 | 为手机竖屏 SSH 优化的会话管理器 | 数字键直选、自动命名、attached 冲突对话框、detach 回到 TUI 的循环 | 需 Node 18+/npm；无预览；无环境诊断 |
| 3 | [pouch](https://github.com/zpdldhkdl/pouch) | Rust（npm 分发预编译二进制） | session-first 面板 + 可脚本化 CLI | **managed/unmanaged 分离**、`doctor` 自检、危险操作二次确认 | 功能偏保守（无预览）；需要 Screen 额外 UTF-8/terminfo 配置才不糊 |
| 4 | [tscreen](https://github.com/mdrobniu/tscreen) | Node ≥18，**零运行时依赖** | 按「会话在干什么」而不是 PID 来浏览 | 窄屏 72 列阈值、宽度 clamp、`hardcopy` 实时预览、`ls` 纯文本模式 | 语义标题依赖 Linux `/proc`，macOS 退化；附带 autopilot 会向活会话注入按键；零依赖只指 npm 依赖，仍需 Node |
| 5 | [screen-manager](https://github.com/TapsHTS/screen-manager) | Python 3.8+ 单文件 | 全功能 TUI：概览 + 信息面板 + 预览 + 动作弹窗 | 鼠标支持、就地重命名、预览键、5s 自动刷新 | 单文件脚本，可测性差；需 Python 3.8+；无元数据持久化 |
| 6 | [scrn](https://github.com/jensbech/scrn) | Rust | 把 Screen 包成带工作区树的管理台 | 启动即校验 Screen 版本、**内嵌 PTY 显示**、workspace 模式、模糊搜索 | 强制 Screen 5.0+（为真彩）；实现重（自带终端模拟）；配置在 TOML |

## 2. 逐个分析

### 2.1 spv — Screen Process Viewer

**做的事**：打开即双栏界面，左列表右详情；每秒自动刷新 CPU / RAM / 会话状态。

**功能点**
- 列表：会话 ID、状态（Attached/Detached）、autostart 配置、运行的命令、自定义描述
- 新建走多步向导：Name → Command → Description
- 会话的命令与描述通过 JSON 配置文件跨重启保留
- `t` 键在已有会话上切换 autostart（Linux 会代管 systemd 服务文件；macOS/Windows 明确不支持）
- 9 套内置主题（slate/pink/forest/mellow/arctic/solarized/dracula/gruvbox/nord），`spv theme <name>` 持久化
- 页脚常驻快捷键提示；顶部会显示本仓库最新 commit message

**按键**：`↑↓` 选择、`Enter` 连接、`a` 新建、`k` 杀掉、`r` 刷新、`t` 切换 autostart、`?` 关于、`q` 退出。

**可借鉴**：向导式新建（把「名称/命令/描述」拆步，比一屏塞满表单更适合小屏）；主题机制；把元数据（描述、启动命令）持久化。

**要警惕**：启动时去 GitHub 拉最新 commit 做动态标题 —— 对一个服务器工具来说这是不必要的外网依赖与启动延迟，不采纳。autostart 代管系统服务属于越权重操作，v1 不做。

### 2.2 sm — Screen Manager (npm: `screen-manager-tui`)

**做的事**：SSH 登录后自动弹出的会话管理器，为手机竖屏优化。

**界面（README 原文示例）**
```
 sm (3)
 > 1 🌻 ● sm-0412-1249    12:49
   2 🔥 ● llm-0412-1300   13:00
   3 🐳 ○ work-0411-0900  09:00
 ────────────────────────────
   4 + sm claude
   5 + sm
   6 + llm claude
   7 + llm
 1-9 go  jk sel  n new  x kill  r ref  q quit
```

**功能点**
- 首页分两区：已有会话 / 快速新建
- 已有会话：基于名称哈希的**固定 emoji**（同一会话每次显示一致）、状态 `●` 可连 `○` 已占用 `✕` 已死、`1`-`9` 数字键直选
- 快速新建：最近使用 top 2 收藏目录 × 是否启动 Claude Code = 4 个一键入口
- 新建流程：选目录（`1`-`9`/`jk`/`Tab` 切 claude 开关/`Esc` 返回），会话名自动生成 `目录名-月日-时分`
- 收藏目录显示完整路径，过长自动缩写（`/h/z/p/subfolder`）；自定义路径自动入库
- **attached 冲突处理**：选中已占用会话时弹出 `1 共享会话（多屏同显） / 2 强制接管（踢掉另一端） / Esc 取消`
- **核心交互循环**：`SSH 登录 → sm 启动 → 选择/新建会话 → 进入 screen`，`Ctrl-A D` 断开后**自动回到 sm 界面**，不需要重新输命令
- SSH 自动启动：`~/.zshrc` 末尾判断 `$SSH_CLIENT` 且 `$STY`/`$SM_SKIP` 为空才启动
- 数据存 `$SM_HOME/.sm-data.json`
- 项目结构清晰：`components/`（SessionList / NewSession / AttachModeDialog / ConfirmDialog / Header / Footer）、`hooks/useScreenSessions`、`utils/screen.ts` + `utils/dirs.ts`

**可借鉴**：数字键直选（手机操作的第一生产力）；自动命名；attached 冲突对话框（共享 vs 接管，把 `-x` 和 `-d -r` 的语义讲清楚）；`Ctrl-A D` 回到 TUI 的循环是**所有同类工具的共同契约**；UI 与 screen 命令封装分离的分层。

**要警惕**：`utils/screen.ts` 的解析正则曾因 macOS 的 `-ls` 格式（无日期列）出错 —— 与我们实测到的差异完全一致，说明**解析器必须一开始就按多版本设计**。依赖 Node 18+ 与 npm 分发包，对「服务器上随手用」略重。

### 2.3 pouch — session-first manager + CLI

**做的事**：打开先给一块「会话看板」，需要脚本化时再退回传统 CLI 子命令。

**关键概念：managed vs unmanaged**
- **Managed**：由 pouch 创建，记录原始命令与工作目录，可从 TUI 或 CLI 重启
- **Unmanaged**：已存在的 Screen 会话，保持可见可连接，但**禁用重启**

**CLI 子命令**：`pouch run <name> -- <cmd>`、`pouch ls`、`pouch attach <name>`、`pouch restart <name>`、`pouch stop <name>`、`pouch doctor`、`pouch --version`。

**TUI 按键**：`↑↓` 选择、`Enter` 连接、`/` 过滤（按名字或命令）、`n` 新建、`c` **克隆选中会话**（预填表单）、`s` 停止（需确认）、`r` 重启（需确认）、`g` 刷新、`?`/`h` 帮助、`q` 退出。

**安全设计（值得抄）**
- stop / restart 都弹确认框，`Enter`/`y` 确认，`Esc`/`n` 取消
- **每次 attach 前打印 detach 提示**（默认 `Ctrl-A d`），并说明若改过转义前缀该怎么按
- `doctor` 检查：平台支持、screen 可执行文件、`screen -ls` 可用性、会话元数据健康、`screen-256color` terminfo 是否存在、是否有早于 UTF-8 配置的遗留会话

**踩坑记录（对我们直接有用）**
- 用 `screen -U` 启动、优先 `screen-256color`，否则 HUD 的分隔符/图标会糊
- 建议用户 `.screenrc` 里加 `defutf8 on` 与 `term screen-256color`
- 已有遗留会话必须 `pouch restart <name>` 重建才能修正显示

**可借鉴**：managed/unmanaged 的区分（**这是本工具「不接管别人的会话」这条边界的现成实现范式**）；`doctor`；克隆；确认式危险操作；detach 提示。

### 2.4 tscreen — 按语义浏览

**做的事**：解决 `screen -ls` 只给 PID 和 tty 的问题 —— 「到底哪个会话在修登录崩溃？」

**核心机制（三段）**
1. 沿进程树找到会话里的实际负载（`claude` 进程或其它），从 `/proc` 读其 `cwd`
2. 把 cwd 映射到 `~/.claude/projects/<munged-cwd>`，读最新 transcript 的尾部，取最后一条 prompt 当标题
3. 预览用 `screen -X hardcopy` —— **不 attach 就能拿到可见屏幕快照**

列表按最近活动排序；除 `hardcopy` 落临时文件（退出时清理）外，**不向会话写任何东西**。

**窄屏适配（README 明确写了）**
- 宽终端：左右双栏（左列表 / 右实时预览）
- **窄终端（手机 SSH 客户端如 Termius，或任何 < 72 列）：自动改为上下堆叠** —— 列表在上、较短的预览在下、footer 精简
- **禁用自动换行**，每行做宽度裁剪（emoji 感知），保证布局回流时不会错行

**按键**：`↑↓`/`jk` 移动、`g`/`G` 首尾、`↵` 连接（用 `screen -x`）、`/` 过滤（项目/prompt/cwd/会话 ID）、`d` 断开其它客户端并把尺寸重置为当前终端、`r` 刷新、`x` 终止（需 `y` 确认）、`q`/`Ctrl-C` 退出。

**脚本化**：`tscreen ls` 输出与界面相同的纯文本信息，便于管道处理。

**autopilot（可选附件，我们不抄）**：cron 每分钟跑一次，对「有活跃 `/goal` + 空闲 + 没在等人」的会话自动接受 ghost 建议（右方向键补全 + Enter）；带一堆安全默认值（**已 attached 的会话绝不碰**、只对 `/goal` 会话生效、`esc to interrupt` 的忙碌会话跳过、`TSAP_DRY` 干跑）与环境变量旋钮，状态存 `~/.cache/tscreen-autopilot.json`，日志轮转 512KB。

**可借鉴**：72 列这个具体阈值（有实测依据）；宽度裁剪要 emoji 感知；`hardcopy` 预览；`ls` 纯文本模式；`screen -x` 做多端共享。

**要警惕**：语义标题强依赖 Linux `/proc` 与 Claude Code 的私有目录结构，macOS 上退化为进程名。我们的方案要把这一层做成**可选增强**，拿不到就优雅留空。autopilot 向活会话注入按键的行为，本工具明确不做。

### 2.5 screen-manager — 功能最全的单文件 Python TUI

**做的事**：单个 `screen_manager.py`，把所有能想到的操作都放上。

**功能点**
- 集中概览：PID、名称、创建日期、状态（Attached / Detached / **Multi**）
- 选中会话的实时信息面板
- **实时预览**：`p` 打印回滚缓冲，无需 attach
- **动作弹窗**：`Enter`/`Space` 弹出上下文菜单
- 连接：`a` = `screen -r`（独占）；`x` = `screen -x`（共享）
- `n` 一键新建命名会话；`r` 就地重命名运行中的会话
- `d` 远程断开（不 attach）；`K` 删除（带确认）；`R` 手动刷新
- **鼠标支持**：单击选中，再次单击打开弹窗
- 每 5 秒自动刷新
- 每次 attach 前打印 `Ctrl+A D` 返回提示
- 依赖：Python 3.8+、GNU screen（tmux 仅测试用）

**可借鉴**：状态里显式区分 `Multi`（对应手册里的 multi 标记）；`p` 预览；就地重命名；自动刷新 5s 这个量级；`Ctrl+A D` 提示。

**要警惕**：单文件、无测试、无元数据持久化、无版本兼容处理 —— 恰恰是我们应当在架构上做得更好的地方。鼠标支持对手机价值有限，列为可选（且只能依赖 4.7+ 的 SGR 1006）。

### 2.6 scrn — 版本门禁 + 工作区模式

**做的事**：把 Screen 包成一个带工作区树的管理台，并在启动时卡版本。

**关键点**
- **要求 GNU Screen 5.0+**（为真彩支持），启动时自行校验并明确告知版本过低 —— macOS 上建议 `brew install screen`
- 交互式表格浏览；创建/重命名/杀会话
- **会话间无缝跳转，不产生嵌套**
- 模糊搜索
- **内嵌 PTY 显示**：连接会话时在其自己的 PTY 里显示，因此能在会话周围继续渲染自己的 UI
- **shell 集成**：zsh 与 bash
- **workspace 模式**：指向一个装满 git 仓库的目录 → 树状展示；选中仓库打开左右分栏（左：该仓库的 Screen 会话；右：配套会话，如编辑器+终端并排）；首次打开自动创建，之后自动重连。配置在 `~/.config/scrn/config.toml` 的 `workspace` 字段，或 `scrn -w <dir>`
- 按键：列表 `j/k`、`g/G`、`Enter` 连接、`c` 新建、`x` 杀、`X` 全杀、`o` 切换已打开过滤、`d` 回首页、`/` 搜索、`r` 刷新、`?` 图例、`q` 退出；会话内 `Esc Esc` 断开、`Ctrl+S` 交换分栏、`Ctrl+A,D` 标准断开

**可借鉴**：**启动即做版本门禁**并给出可执行的升级建议；「不嵌套跳转」的体验目标；`d` 回首页这种「回到管理界面」的一等公民键位。

**要警惕**：内嵌 PTY 等于要自带终端模拟器 —— 依赖体积、VT 序列兼容性、性能都会上一个量级。这与「服务器上零依赖随手可用」相冲突，v1 不采用（列为开放问题）。强制 5.0+ 的代价是放弃大量仍在跑 4.x 的服务器，我们选择**反向策略：兼容到 4.00.03，能力缺失就降级**。

## 3. 功能覆盖矩阵

`●` = 支持；`○` = 部分/需额外条件；`—` = 不支持

| 能力 | spv | sm | pouch | tscreen | screen-manager | scrn |
| --- | :-: | :-: | :-: | :-: | :-: | :-: |
| 会话列表 | ● | ● | ● | ● | ● | ● |
| 新建会话 | ● | ● | ● | — | ● | ● |
| 连接（独占） | ● | ● | ● | ● | ● | ● |
| 共享连接 `-x` | — | ● | — | ● | ● | ○ |
| 强制接管 `-d -r` | — | ● | — | — | — | — |
| 远程断开 `-d` | — | — | — | ● | ● | — |
| 终止会话 | ● | ● | ● | ● | ● | ● |
| 终止前确认 | — | ○ | ● | ● | ● | — |
| 重命名 | — | — | — | — | ● | ● |
| 会话预览（不 attach） | — | — | — | ● | ● | — |
| 过滤 / 搜索 | — | ○ | ● | ● | — | ● |
| 自动刷新 | ● | — | ○ | ○ | ● | ○ |
| 小屏专用布局 | — | ● | — | ● | — | — |
| 鼠标支持 | — | — | — | — | ● | ○ |
| 元数据持久化 | ● | ● | ● | — | — | ○ |
| 收藏目录 / 最近使用 | — | ● | — | — | — | ● |
| 环境自检 doctor | — | — | ● | — | — | ○(仅版本) |
| 纯文本输出模式 | — | — | ● | ● | — | — |
| 主题 | ● | — | — | — | — | — |
| 版本兼容处理 | — | ○ | ○ | ○ | — | ●(卡版本) |
| 自动命名 | — | ● | — | — | — | — |
| 危险操作二次确认 | — | — | ● | ● | ● | — |
| managed/unmanaged 区分 | — | — | ● | — | — | — |
| 语义化标题 | — | — | — | ● | — | — |

## 4. 从参考项目提炼出的需求结论

### 4.1 已是共识（不做就是缺功能）

1. **会话列表 + 状态标识** —— 六个项目全部具备。
2. **连接后 detach 要回到本工具**（而不是退回 shell 让用户重输命令）—— `sm` 把它写成显式的「核心交互循环」，`pouch`、`tscreen`、`screen-manager`、`scrn` 也都有。*（v0.2 二修：本工具改为 **exec 替换进程**，detach 回原 shell —— spawn 方案下 screen 客户端在 TUI 子进程环境里实测接管后秒退 code 1，且 exec 省去整条「回来重绘」链路；参考项目多spawn 子进程方案的前提是它们不做 TUI 落账。）*
3. **每次连接前提示 `Ctrl-A D`** —— `pouch` 与 `screen-manager` 都专门做了，`pouch` 还考虑到了用户改过转义前缀的情况。
4. **危险操作必须二次确认** —— 三个项目独立做了同一件事。
5. **新建会话要支持指定目录与启动命令**，且会话名要可读（`sm` 用 `目录名-月日-时分` 自动命名，`pouch` 用 `managed` 记录原始命令与 cwd）。

### 4.2 有明显分歧（需要我们自己拍板）

| 议题 | 立场 A | 立场 B | 建议 |
| --- | --- | --- | --- |
| 连接实现 | 让出终端直接 exec `screen`（spv/sm/pouch/tscreen/screen-manager） | 内嵌 PTY 自己渲染（scrn） | **A**。B 会让项目从「小工具」变成「终端模拟器」，与零依赖目标冲突 |
| 目标 Screen 版本 | 卡 5.0+ 换真彩（scrn） | 兼容老版本、能力降级 | **B**。4.x 仍是服务器主流；且实测 4.00.03 已支持本工具所需的绝大部分能力，只缺 `-Q` |
| 是否操作外部会话 | 只连接，不接管（spv） | managed/unmanaged 分离，只重启自己的（pouch） | **B**。既尊重用户已有会话，又给自建会话更强的重启能力 |
| 是否注入按键 | 明确不注入（tscreen 主程序） | 自动补 `continue`（tscreen autopilot） | **A**。静默改写用户正在跑的 AI 会话风险过高 |
| 元数据存哪 | 项目内的 JSON（spv/sm） | 系统标准配置目录 | 系统标准目录（`XDG`），保证多目录启动也一致 |

### 4.3 尚无人做好（差异化机会）

1. **统一的版本兼容与能力探测**。`sm` 是被 macOS 格式打脸后才修的，`scrn` 直接放弃老版本，`screen-manager` 完全没处理。**做成「启动自检 + 能力矩阵 + 优雅降级」是清晰的空白点。**
2. **老版本上的预览**。预览能力事实上只依赖 `hardcopy`，而 `hardcopy` 连 4.00.03 都有；但只有 `tscreen` 与 `screen-manager` 做了。**在 4.x 服务器上也能预览，是低成本高感知的差异点。**
3. **小屏的「信息分级」而不只是「布局切换」**。`tscreen` 做了堆叠与裁剪，但没做内容级取舍。README 明确要求「小屏时只显示少量必要信息（比如不显示帮助信息）」—— **信息分级（同一功能在不同尺寸下暴露不同粒度）还没人做。**
4. **中英双语**。`screen-manager` 做了法英双语，其余以英文为主。面向中文用户的服务器工具多语言可选项很少见。
5. **可测试性**。六个项目里没有一个提到解析层的单元测试 —— 而这恰恰是 `-ls` 格式差异最容易出事的地方。
