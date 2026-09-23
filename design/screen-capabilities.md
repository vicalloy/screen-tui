# GNU Screen 能力边界分析

> 目的：确定「哪些能力可以由 GNU Screen 原生提供、哪些必须由本工具自建」。
> 结论来源分两类：**[手册]** 来自 GNU Screen 官方手册（4.9.1 / 5.0 系列）；**[实测]** 来自本机 GNU Screen 4.00.03 的实跑记录（原始命令与输出见文末附录）。

## 1. 为什么是 Screen 而不是 tmux

| 维度 | GNU Screen | tmux |
| --- | --- | --- |
| 服务器预装率 | 极高，几乎默认存在 | 较高，但常需另装 |
| 老系统 / 受限环境 | 4.00.03 这类 2006 年的版本仍广泛存在（macOS 自带即是） | 老版本兼容性差 |
| 外部控制面 | `-X` 可注入命令，`-ls` 可枚举，`hardcopy` 可截屏，**不需要 attach** | 类似能力需 `capture-pane` 等服务端配置 |
| 协议 | socket 文件 + 文本输出，易解析 | 控制模式更结构化，但实现更重 |

结论：Screen 的**外部控制面已经够用**，本工具不需要重造终端复用器。核心价值在于把这套晦涩的命令行接口映射成一个体面的界面。

## 2. 可用的三层控制面

### 2.1 第一层：一次性命令行调用（进程外，最可靠）

| 用途 | 命令 | 备注 |
| --- | --- | --- |
| 枚举会话 | `screen -ls` / `screen -list` | 文本输出，格式随版本变化，见 §4 |
| 结构化枚举判定 | `screen -q -ls` | **退出码即状态**，比解析文本更稳，见 §4.3 |
| 创建并分离 | `screen -dmS <name> [cmd]` | 最常用的创建方式 |
| 创建（前台并进入） | `screen -S <name> [cmd]` | 注意与 `-r` 语义区分：只有 `-r` 才是「连接」 |
| 连接分离会话 | `screen -r <name>` | 名字不唯一时需给全名 `<pid>.<name>` |
| 连接并抢先断开对方 | `screen -d -r <name>` | 踢掉已连接的一端后接管 |
| 连接并让对端登出 | `screen -D -r <name>` | `-D` = power detach，会对父进程发 HUP |
| 共享连接 | `screen -x <name>` | 多显示器模式，手机与电脑可同时看同一会话 |
| 仅断开 | `screen -d <name>` | 不进入会话，直接让对方 detach |
| 自适应窗口尺寸 | `-A` 与 `-r` 同用 | 解决「多客户端下尺寸被压成最小值」问题 |
| 清理死会话 | `screen -wipe` | 输出同 `-ls`，但会真正删除 dead 项 |
| 版本探测 | `screen -v` | 输出形如 `Screen version 4.00.03 (FAU) 23-Oct-06` |

### 2.2 第二层：向运行中的会话注入命令（`-S <name> -X <cmd>`）

`-X` 是会话管理的核心通道。常用命令：

| screen 命令 | 作用 | 本工具用途 |
| --- | --- | --- |
| `sessionname <name>` | 重命名会话（等价于 `-S`） | 会话重命名 |
| `title <name>` | 设置当前窗口标题（需 `-p` 指定窗口） | 窗口级命名 |
| `detach` | 断开连接，会话继续运行 | 「不进入直接断开」 |
| `quit` | 杀掉所有窗口并终止会话 | 终止会话 |
| `kill` | 销毁当前窗口 | 慎用：会连带终止会话 |
| `hardcopy [-h] [file]` | 把当前窗口内容写出到文件，`-h` 含回滚缓冲 | **会话预览的唯一无侵入手段** |
| `hardcopydir <dir>` | 设置 hardcopy 落盘目录 | 隐私路径控制 |
| `hardcopy_append <state>` | 追加而非覆盖 | 一般不用 |
| `stuff <string>` | 向窗口输入缓冲注入字符串 | **本工具明确不使用**，见 §7 |
| `windows` | 列出窗口 | 多窗口会话概览 |
| `select .` / `select -` | 切换到当前/空白窗口 | 配合 `-p` 使用 |
| `eval <cmd...>` | 一次发送多条命令 | 批量化 |
| `at ... <cmd>` | 在指定显示/窗口执行命令 | 一般不用 |
| `log` / `logfile <file>` | 开关会话日志 | 可选功能 |
| `wall <msg>` | 向所有显示广播消息 | 一般不用 |

### 2.3 第三层：远程查询（`-Q <cmd>`）—— 版本门槛最高

手册原文：*"Some commands now can be queried from a remote session using this flag, e.g. 'screen -Q windows'. The commands will send the response to the stdout of the querying process."*

可查询命令（手册完整列表）：`echo`、`info`、`lastmsg`、`number`、`select`、`time`、`title`、`windows`。

**关键区别**：`-X windows` 的输出是发给**会话自己的显示器**（本工具看不到），而 `-Q windows` 的输出**回到调用进程的 stdout**（可直接读）。因此凡是需要读取返回值的信息（窗口列表、标题、编号、状态），只有 `-Q` 可用。

**但 `-Q` 在老版本上不存在。** [实测] 本机 4.00.03 执行 `screen -Q windows` 直接报 `Error: Unknown option -Q`，并打印完整 usage。旁证：4.6.1 的发版说明包含 *"segfault when querying info on nonUTF locale"*，说明 `-Q info` 至迟在 4.6 系列已存在。

→ 需求结论：**`-Q` 只能作为增强路径，必须有降级方案**（见 §4.3 与 §5）。

## 3. 会话状态模型

手册对 `-ls` 输出中状态标记的原文定义：

| 标记 | 含义 |
| --- | --- |
| `detached` | 无控制终端，可用 `screen -r` 恢复 |
| `attached` | 正在运行且已有控制终端 |
| `multi` | 多用户模式下运行的会话 |
| `unreachable` | 位于其它主机或已死；当名字匹配本地主机名时视为 dead |
| `dead` | 应检查并移除，用 `-wipe` 清理 |

对本工具的业务映射：

| 界面状态 | 来源 | 可执行操作 |
| --- | --- | --- |
| 可用（Detached） | `-ls` → detached | 连接 / 共享连接 / 重命名 / 终止 / 预览 / 断开 |
| 已占用（Attached） | `-ls` → attached | 共享连接 / 强制接管 / 终止 / 预览 / 断开 |
| 多端（Multi） | `-ls` → multi | 同「已占用」，另提示多端尺寸风险 |
| 已死（Dead） | `-ls` → dead | 仅提示与 `-wipe` 清理，禁止连接 |
| 不可达（Unreachable） | `-ls` → unreachable | 只读展示 |

## 4. `-ls` 输出格式与解析

### 4.1 两种格式（这是最坑的地方）

**[实测] 4.00.03 / macOS 自带（无日期列）**：

```
There is a screen on:
	11121.ttys002.MacBook-Pro-3	(Detached)
1 Socket in /var/folders/vp/hvhc4_b90s92slrx__l_42kw0000gn/T/.screen.
```

**[手册/社区] 4.6+ 常见格式（含创建时间列）**：

```
There are screens on:
	12345.work	(08/09/2026 10:23:45 AM)	(Attached)
	67890.llm	(08/09/2026 11:02:00 AM)	(Detached)
2 Sockets in /run/screen/S-user.
```

差异要点：
1. 单/复数表头：`There is a screen on:` vs `There are screens on:`。
2. 是否有 `(MM/DD/YYYY HH:MM:SS AM)` 日期列 —— 参考项目 `m2kar/sm` 的提交记录里专门有一条 *"Fix screen -ls regex to handle macOS format (no date field)"*，说明这是真实踩坑点。
3. 结束行：`N Socket(s) in <dir>.`；无会话时为 `No Sockets found in <dir>.`。
4. 字段以 **Tab** 分隔，会话标识形如 `<pid>.<name>`（默认命名则为 `<pid>.<tty>.<host>`）。
5. `.screenrc` 的 `sort` 选项会改变行序，不能假定有序。
6. **[实测补充] 输出走 stdout，不是 stderr。** 4.00.03 实测 `screen -ls >o 2>e`：`o` 有内容、`e` 为空。想当然去读 stderr 会拿到空列表。
7. **[实测补充] 行尾是 `\r\n`，且末尾多一个空行。** 实测字节为 `...No Sockets found in <dir>.\n\r\n`。解析前不去掉 `\r`，状态词匹配（如 `(Detached)\r`）就匹配不上 —— 这是「列表看起来解析成功但状态全是未知」的隐蔽故障源。
8. **[实测补充] `screen -v` 即使成功也返回退出码 1**（`--version` 同样）。版本探测**不能**用退出码判断成败，只能解析文本。

→ 解析器要求：**不要把日期列当作必填**。正则应为「会话标识 + 可选日期 + 括号内状态」的宽松匹配，且必须用多版本样本做回归测试。

### 4.2 名称解析规则

- 默认命名：`<pid>.<tty>.<host>`
- `-S <name>` 命名后：`<pid>.<name>`
- 同名会话多于一个时，`screen -r <name>` 会给出候选列表并要求指定全名 → 本工具在连接时必须能回退到 `<pid>.<name>` 形式。

### 4.3 `-q -ls` 的退出码 —— 只能当快路径，不能当事实来源

手册（4.9/5.0 系列）对 `-q` 与 `-ls` 组合的约定：

| 退出码 | 含义 |
| --- | --- |
| 9 | 目录中没有会话 |
| 10 | 有正在运行但不可连接的会话 |
| 11+ | 有 1 个及以上可用会话（数值-10 即数量） |

配套的 `-r` 组合退出码：10 = 无可恢复会话，12+ = 存在 2 个及以上可恢复会话（必须指定）。

**但 [实测] 这套表在 4.00.03 上不成立。** macOS 自带 4.00.03、socket 目录为空（`ls -la` 确认 0 个文件）时：

```
$ screen -q -ls >/dev/null 2>&1; echo $?
8          # 三次重复均为 8，不是手册说的 9
$ screen -ls >/dev/null 2>&1; echo $?
1
```

原因推测：4.00.03 的 `-q` 语义就是 usage 里那句 *"Quiet startup. Exits with non-zero return code if unsuccessful."* ——
**只区分「成/败」，没有 9/10/11+ 的细分**；细分是后来版本才加的。

→ 修正后的需求结论（取代原来的「退出码优先」）：

1. 退出码**只作快路径**：命中 9 / 10 / 11+ 时可直接采信，省一次解析；
2. 不在表内的值（如 8、0）视为**不可判定**，必须退回 `-ls` 文本明细；
3. 两者矛盾时**以文本明细为准**（文本是明细的唯一事实来源），并把矛盾如实记入诊断输出；
4. 因此 `-ls` 解析器是**必经路径**而非可选优化 —— 它必须自己足够健壮，不能指望退出码兜底。

这条修正直接改写了 FR-19 的验收标准（见 `requirements.md`），也是 M0 唯一的「需求回改」。

## 5. 版本能力矩阵

| 能力 | 4.00.03（本机实测） | 4.6+ | 4.9.x | 5.0+ |
| --- | --- | --- | --- | --- |
| `-ls` / `-list` | ✅ | ✅ | ✅ | ✅ |
| `-ls` 含创建时间列 | ❌ 无 | ✅ 有 | ✅ 有 | ✅ 有 |
| `-q -ls` 退出码语义 | ❌ 实测 8，非手册的 9（见 §4.3） | 待验证 | ✅ 手册收录 | ✅ 手册收录 |
| `-Q` 远程查询 | ❌ 实测 `Unknown option -Q` | ✅ 至少 `info` 可用 | ✅ | ✅ |
| `-X` 命令注入 | ✅（机制在，见附录说明） | ✅ | ✅ | ✅ |
| `hardcopy` / `-h` | 机制在，实测未通 | ✅ | ✅ | ✅ |
| `sessionname` | 机制在 | ✅ | ✅ | ✅ |
| `-wipe` | ✅ 选项存在 | ✅ | ✅ | ✅ |
| `-dmS` 创建 | ✅ 实测成功 | ✅ | ✅ | ✅ |
| `-U` UTF-8 | ✅ 选项存在 | ✅ | ✅ | ✅ |
| `-x` 共享连接 | ✅ 选项存在 | ✅ | ✅ | ✅ |
| `-d -r` / `-D -r` | ✅ 选项存在 | ✅ | ✅ | ✅ |
| socket 实现 | FIFO 时代 | **4.6.0 起由 FIFO 迁移为 socket** | socket | socket |
| SGR 1006 鼠标 | ❌ | ❌ | ✅ 4.7.0 起 | ✅ |
| truecolor | ❌ | ❌ | ❌ | ✅（`scrn` 明确要求 5.0+） |
| 同步多窗口输入 | ❌ | ❌ | ❌ | ✅ 5.0.0 新增 |

版本发布时间线（用于判断目标环境可能落在哪一档）：4.0.3 = 2008-08-07；4.5.0 = 2017-01；4.6.0 = 2017-06；4.8.0 = 2020-02；4.9.0 = 2022-02；**5.0.0 = 2024-08-28**。

两条重要推论：
1. **socket 实现在 4.6.0 换代**，因此 4.00.03 与 4.6+ 在 socket 目录位置、权限检查、`-ls` 输出三条线上都不同。macOS 自带的 4.00.03 属于「上古档」，必须单独兼容。
2. **颜色不能假设**。想做真彩界面必须先探测 5.0+，否则退化为 16/256 色。

## 6. 本机实测记录（原始证据）

环境：macOS，`/usr/bin/screen`，`Screen version 4.00.03 (FAU) 23-Oct-06`。

| # | 探测项 | 命令 | 结果 | 判定 |
| --- | --- | --- | --- | --- |
| 1 | 版本 | `screen -v` | `Screen version 4.00.03 (FAU) 23-Oct-06` | ✅ |
| 2 | 枚举现状 | `screen -ls` | `11121.ttys002.MacBook-Pro-3 (Detached)`，无日期列 | ✅ 确认老格式 |
| 3 | 创建分离会话 | `screen -dmS __stui_probe__ sleep 300` | 返回码 0，且立刻出现在 `-ls` 中 | ✅ 创建链路可用 |
| 4 | `-Q` 查询 | `screen -Q windows` | `Error: Unknown option -Q` + usage，返回码 1 | ✅ 确认不可用 |
| 5 | `-X sessionname` | `screen -S <name> -X sessionname <new>` | `No screen session found.`，返回码 1 | ⚠️ 会话已被回收，见下 |
| 6 | `-X hardcopy` | `screen -S <name> -X hardcopy /tmp/hc.txt` | 同上，文件未生成 | ⚠️ 同上 |
| 7 | `-X windows` | `screen -S <name> -X windows` | 同上 | ⚠️ 同上 |
| 8 | `-X detach` / `-X quit` | 同上 | 同上 | ⚠️ 同上 |
| 9 | 会话存活 | 同一脚本内两次 `-ls` | 新建的会话在毫秒级内消失，仅剩用户原有会话 | ❌ 无法在 Agent 沙箱内验证 |

### 6.1 M0 实现期的补充实测（2026-09-23，同机 4.00.03）

写 `screen::parse` / `screen::caps` 时为了确定实现细节又测了一轮，四项结果直接改变了实现：

| # | 探测项 | 命令 | 结果 | 对实现的影响 |
| --- | --- | --- | --- | --- |
| 10 | `-ls` 走哪个流 | `screen -ls >o 2>e` | **stdout** 有内容、stderr 空 | 读 stdout（原文档未声明） |
| 11 | 行尾字节 | `od -c` | `...No Sockets found in <dir>.\n\r\n` | 解析前必须 strip `\r` |
| 12 | `-q -ls` 空列表退出码 | `screen -q -ls; echo $?` | **8**（三次重复），非手册 9 | 退出码降级为快路径（§4.3） |
| 13 | `-v` 的退出码 | `screen -v; echo $?` | **1**（成功也返回 1） | 版本探测只看文本，不看退出码 |
| 14 | `-Q windows` 的失败输出 | `screen -Q windows >o 2>e` | usage dump 打到 **stdout**，退出码 1 | `-Q` 能力判定必须扫 stdout 里的 `Unknown option` / `Use: screen` |
| 15 | `screen --version` | 同 13 | 同样可用，同样返回 1 | `-v` 与 `--version` 都行 |

**必须如实说明的限制**：本 Agent 运行环境中，新建的 detached 会话会被立即回收（同一脚本的连续两条命令之间即消失），因此 **探测项 5–8 全部未得到有效验证**，标记为**待验证**。这些语义本身在 GNU Screen 4.00.03 中是有文档依据的（`-X` 选项确实存在，见附录 usage 原文），但**必须在一台真实的服务器上补测**，测试清单见 §8。

M0 期间对 5–8 采用的是**替身验证**：用一个脚本伪造 `screen` 二进制（`$STUI_SCREEN` 注入），分别模拟 4.00.03（`-Q` 与 hardcopy 都 dump usage）与 4.6+（`-Q`/hardcopy 正常）两种行为，
以此走通 `doctor` 的降级与成功两条路径。这能验证**本工具的判定逻辑**，但**不能替代**真实 `-X` 语义的实测 —— T0.6 仍然开着。

另一个与 `$SCREENDIR` 有关的观察：本机环境里 `SCREENDIR` 被导出为**空值**，而 4.00.03 在这种情况下仍然正常工作（回退到 `$TMPDIR/.screen`）。
这与下面那段「空值导致所有调用失败」的记录并不矛盾 —— 不同构建/版本对空值的处理不一致，所以 `doctor` 把它报成 `Warn` 而不是 `Fail`，并在实现里对所有环境变量一律「空值按未设置处理」。

另有一个 watch out：[实测] 我曾把 `SCREENDIR` 误导出为空值，导致此后所有 `screen` 调用报 `Cannot access : No such file or directory`。这说明 **socket 目录是硬依赖**，任何环境变量处理都必须先判空。

## 7. 陷阱清单

| # | 陷阱 | 后果 | 应对 |
| --- | --- | --- | --- |
| 1 | `-X <cmd>` 的输出不会回到 stdout | 想读窗口列表/标题却读不到 | 读值一律走 `-Q`；不可用时降级为「不展示」而非猜测 |
| 2 | `-Q` 老版本不支持 | 直接崩 | 启动时探测版本，缺失则关闭相关 UI |
| 3 | `-ls` 格式随版本/`sort` 变化 | 解析错乱 | 宽松正则 + `-q -ls` 退出码兜底 + 多版本回归样本 |
| 4 | 会话名不唯一时 `-r` 会拒绝 | 连接失败 | 自动回退 `<pid>.<name>` 全名 |
| 5 | Screen 拒绝从自己内部 attach | 在 screen 里再起本工具会失败 | 检测 `$STY`，提示或改用 `-d -r` |
| 6 | 多客户端会话尺寸取最小值 | 手机上打开后把电脑端画面挤窄 | 提供「自适应尺寸」（`-A`）与「断开其它客户端」（`-d`）两条路径 |
| 7 | dead 会话长期残留 | 列表污染、易误操作 | 状态识别 + `-wipe` 一键清理（需确认） |
| 8 | 密码保护的会话 `-X` 不可用 | 命令静默失败 | 手册明确「doesn't work if the session is password protected」→ 需检查并给出明确报错 |
| 9 | multiuser 会话用 `username/sessionname` 寻址 | 普通寻址找不到 | v1 明确不支持多用户会话，只读展示 |
| 10 | 4.6.0 前后 socket 机制不同 | 目录与权限假设失效 | 不硬编码 socket 路径，从 `-ls` 尾行读取 |
| 11 | `-D` 会对父进程发 HUP | 从登录 shell 启动时可能被登出 | 「强制接管」默认用 `-d -r`，`-D -r` 作为显式危险选项 |
| 12 | `kill` 与 `quit` 语义不同 | 本想杀会话却只杀了一个窗口 | UI 用 `quit` 作为「终止会话」，不做窗口级 kill |
| 13 | `hardcopy` 落盘为会话进程身份 | 权限不符则静默失败 | 落盘路径用会话用户可写的临时目录，读后即删，权限 0600 |
| 14 | `stuff` 会向活会话注入输入 | 可能污染正在跑的 AI 任务 | **本工具禁止使用**（见 §8 非目标） |
| 15 | `-q -ls` 的 9/10/11+ 表在 4.00.03 上不成立（空列表实测返回 8） | 用退出码判断「有没有会话」会得出错误结论 | 退出码只作快路径；不在表内即退回文本明细，矛盾时以文本为准（§4.3） |
| 16 | `-ls` 走 **stdout** 而非 stderr，且行尾是 `\r\n` | 读错流得到空列表；不去 `\r` 则状态词全部匹配失败（列表「解析成功」但状态全是未知） | 读 stdout；解析前 strip `\r` |
| 17 | `screen -v` 成功也返回退出码 1 | `if screen -v; then` 会误判 screen 不可用 | 版本探测只看文本，不看退出码 |
| 18 | `-X` 报错时把完整 usage dump 到 stdout，其中含 `-d (-r)`、`-t title. (window's name).` 这类带括号的行 | 「含括号即会话明细」的宽松规则会把选项说明当成会话 | 准入判据：首个点号前的分量必须是纯数字 pid（`ls-usage-dump.txt` fixture 守这条） |

## 8. 能力 → 需求映射

| 需求 | 可依赖的原生能力 | 降级路径 |
| --- | --- | --- |
| 会话列表 | `-ls` + `-q -ls` 退出码 | 无（必需能力） |
| 新建会话 | `-dmS` | 无（必需能力） |
| 连接会话 | `-r` / `-x` / `-d -r` | 无（必需能力） |
| 会话预览 | `-X hardcopy [-h]` | 不可用时只显示元数据，不造假内容 |
| 窗口级信息 | `-Q windows`（4.6+） | 降级为不展示窗口列表 |
| 会话重命名 | `-X sessionname` | 不可用时隐藏该功能 |
| 终止会话 | `-X quit` | `-wipe` 仅用于 dead |
| 远程断开 | `-X detach` | 无 |
| 工作目录 / 运行命令 | Screen 本身不提供 → 走 `/proc`（Linux）或 `ps`/`lsof`（macOS） | 取不到则留空，不猜测 |
| 自定义别名 / 描述 / 收藏目录 | Screen 不提供 → 本工具自建配置 | — |
| 会话数统计 / 状态图标 | `-ls` + 退出码 | — |

**边界结论**：Screen 能覆盖「生命周期操作 + 只读预览」，但**完全不提供**「工作目录、运行命令、语义标签、收藏、历史」——这些是本工具真正的增量价值所在，也是参考项目之间拉开差距的地方。

## 9. 非目标（明确不做）

1. **不使用 `stuff` 向活会话注入输入。** 参考项目 `tscreen` 的 autopilot 做了这件事（自动给空闲的 `/goal` 会话补 `continue`），但它的 README 自己也强调「Attached sessions are never touched」「Always dry-run it first」。对一个服务于 AI 长任务的工具来说，静默改写用户会话是不可接受的默认行为，v1 明确排除。
2. **不修改用户的 `~/.screenrc`。** 需要 UTF-8 等行为时通过 `-U`、`-c <临时配置>`、`-T screen-256color` 在启动参数上解决；若确实需要用户配合（如 `defutf8 on`），只输出建议，不代写。
3. **不解析 socket 二进制结构**，一律走官方命令行接口。
4. **不内嵌完整终端模拟器**（即不做 `scrn` 那样的 embedded PTY）。理由是会把项目从「零依赖小工具」变成「需要 VT 解析器的中大型程序」，与目标场景（服务器上随手可用）冲突。连接会话采用「让出终端、直接 exec `screen`」的方式。此点列为**开放问题**，见需求文档 §15。

## 附录 A：4.00.03 的 usage 原文（实测）

```
Use: screen [-opts] [cmd [args]]
 or: screen -r [host.tty]

Options:
-a            Force all capabilities into each window's termcap.
-A -[r|R]     Adapt all windows to the new display width & height.
-c file       Read configuration file instead of '.screenrc'.
-d (-r)       Detach the elsewhere running screen (and reattach here).
-dmS name     Start as daemon: Screen session in detached mode.
-D (-r)       Detach and logout remote (and reattach here).
-D -RR        Do whatever is needed to get a screen session.
-e xy         Change command characters.
-f            Flow control on, -fn = off, -fa = auto.
-h lines      Set the size of the scrollback history buffer.
-i            Interrupt output sooner when flow control is on.
-list         or -ls. Do nothing, just list our SockDir.
-L            Turn on output logging.
-m            ignore $STY variable, do create a new screen session.
-O            Choose optimal output rather than exact vt100 emulation.
-p window     Preselect the named window if it exists.
-q            Quiet startup. Exits with non-zero return code if unsuccessful.
-r            Reattach to a detached screen process.
-R            Reattach if possible, otherwise start a new session.
-s shell      Shell to execute rather than $SHELL.
-S sockname   Name this session <pid>.sockname instead of <pid>.<tty>.<host>.
-t title      Set title. (window's name).
-T term       Use term as $TERM for windows, rather than "screen".
-U            Tell screen to use UTF-8 encoding.
-v            Print "Screen version 4.00.03 (FAU) 23-Oct-06".
-wipe         Do nothing, just clean up SockDir.
-x            Attach to a not detached screen. (Multi display mode).
-X            Execute <cmd> as a screen command in the specified session.
```

值得注意：这份 2006 年的 usage 里**没有 `-Q`**，但有 `-A -[r|R]`、`-dmS`、`-S sockname`、`-U`、`-wipe`、`-x`、`-X`。换言之，**本工具需要的能力在 20 年前的版本上基本齐备，只缺 `-Q` 查询**。这是「兼容老版本」策略可行的依据。

## 附录 B：待补测清单（在真实服务器上执行）

以下项在 Agent 沙箱内无法验证（会话被立即回收），需在目标服务器补测并把结果回填进 §6：

| # | 待验证语义 | 建议命令 | 状态 |
| --- | --- | --- | --- |
| 1 | `-S <短名>` 是否能寻址到会话（还是必须 `<pid>.<name>`） | `screen -S <name> -X title T` | 待测 |
| 2 | `-X sessionname` 后 `-ls` 中的名字是否变化 | 改名后 `screen -ls` | 待测 |
| 3 | `-X hardcopy` 与 `-X hardcopy -h` 是否落盘、内容差异 | 比对两个文件行数 | 待测 |
| 4 | `-X detach` / `-X quit` 的实际效果与返回码 | 操作后 `screen -ls` | 待测 |
| 5 | `-q -ls` 在 4.x 上的退出码是否与手册一致 | 分别在有/无会话时取 `$?` | **部分已验证**：无会话时 4.00.03 返回 **8**（非 9），见 §4.3；「有会话时」仍需真实环境 |
| 6 | `-Q windows` / `-Q title` 在 4.6+ 的输出格式 | 4.6+ 环境 | 待测（4.00.03 侧已确认报 `Unknown option -Q`） |
| 7 | 密码保护会话下 `-X` 的报错文本 | `auth on` 后重试 | 待测 |
| 8 | dead 会话的 `-ls` 呈现与 `-wipe` 行为 | 制造 dead 会话 | 待测 |
| 9 | `sort` 选项对 `-ls` 行序的影响 | 修改 `.screenrc` 后对比 | 待测 |
| 10 | 4.6+ 与 4.00.03 的 socket 目录差异 | 对比 `-ls` 尾行 | 待测 |
