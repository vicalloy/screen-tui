# Screen TUI 开发计划

- 版本：v1.0（2026-09-23）
- 输入：`requirements.md`（FR/NFR 与里程碑出口条件）、`tech-design.md`（模块与实施顺序）、`screen-capabilities.md`（附录 B 补测清单）
- 编排原则：**每个任务独立可验收、可提交**；构建链最先趟平；真实服务器补测前移（不等到最后）；计划不含时间估算，只含依赖关系与验收标准。

---

## 1. 里程碑总览

| 里程碑 | 主题 | 出口条件（对应 requirements.md §13） | 状态 |
| --- | --- | --- | --- |
| **M0** | 只读地基 + 构建链 | 多版本 `-ls` fixture 回归全绿；`doctor` 在 4.00.03 与 4.6+ 上结论正确；Docker 出 musl 双架构二进制 | **T0.0–T0.5 完成；T0.6 部分完成；Linux musl 产物待网络条件允许后产出**（见 §2.1） |
| **M1** | P0 核心闭环 | 手机尺寸模拟下走通「看 → 选 → 进」；连接为 exec 替换进程（detach 回原 shell）；`kill -9` 后终端不报废 | 未开始 |
| **M2** | P1 增强 | P0 + P1 全量 FR 验收通过；危险操作均二次确认；预览降级路径可用 | **T2.1–T2.7 编码完成**（2026-09-23，143 tests 全绿）；`-X`/`hardcopy` 真实语义验收随 T0.6 待补 |
| **M3** | P2 打磨 | 按需排期（主题/日志/语义标题/工作区/双语） | 未排期 |

---

## 2. M0 —— 只读地基 + 构建链

| 任务 | 内容 | 依赖 | 验收标准 | 状态 |
| --- | --- | --- | --- | --- |
| ~~T0.0 工程骨架~~ | cargo 工程 + 8 项依赖 + release profile | — | `cargo check`/`build` 通过 | ✅ 2026-09-23 |
| T0.1 构建链 | `Makefile`（build-linux / build-macos / test / lint）+ `scripts/package.sh`；cargo-zigbuild 镜像出 musl 双目标 | T0.0 | ① `make build-linux` 产出 amd64+arm64 两个**静态**二进制；② 产物在 Alpine 容器内 `stui --version` 可执行；③ `make build-macos` 出双架构 | ⚠️ ③ 实测通过；①② 脚本就绪但**未跑通**（镜像 1091 MiB、GHCR 实测约 424 KB/s，见 §2.1） |
| T0.2 能力探测 | `screen::caps`：`-v` 版本解析、`-Q`/`hardcopy` 实探测 → `Caps` | T0.0 | 单测覆盖 `4.00.03`/`4.06.02`/`5.0.2`/非标准输出 4 组样本；代码中无版本号字面量比较 | ✅ 4 组样本 fixture 化并通过；`Support` 三态替代 `bool`；grep 确认无版本号比较 |
| T0.3 会话解析 | `screen::parse`：`-q -ls` 退出码优先 + `-ls` 文本明细两级策略 | T0.0 | fixture ≥ 4 组（无日期/有日期/空列表/dead+unreachable）+ 畸形输入（空行、未知状态、超长名、重名）；解析失败返回 `Err` 不 panic | ✅ 6 组 fixture + 11 个解析单测；退出码改为「快路径 + 文本为准」（需求回改，见 §2.2） |
| T0.4 `stui ls` | 纯文本子命令 + 退出码 0/1/2 | T0.2, T0.3 | 无 ANSI 输出可管道；无 TTY 时不输出控制序列 | ✅ 实测 stdout 中 ESC 字节数为 0；非 TTY 自动切机器格式（`--no-header` 同义） |
| T0.5 `stui doctor` | 11 项环境检查（requirements.md §10） | T0.2 | 每项产出 Pass/Warn/Fail + 修复建议；存在 Fail 时退出码 2 | ✅ 11 项齐全；4.00.03 与 4.6+ 两条路径均给出正确结论（§2.1） |
| T0.6 **服务器补测** | 附录 B 的 10 项语义实测（`-X` 系列、`-q -ls` 退出码、dead 呈现等） | T0.4 | 结果回填 `screen-capabilities.md` §6 并更新 `Caps` 探测逻辑；如有语义偏差，修订受影响 FR | ⚠️ **1/10 已验**（`-q -ls` 无会话退出码 = 8，非 9）；其余 9 项需真实服务器；沙箱内 detached 会话被立即回收，`-X` 语义无法验证 |

M0 出口：`make test` 全绿 + `make build-linux` 产物在真实 Linux 容器可跑 + T0.6 完成。

### 2.1 M0 执行结果（2026-09-23）

**已验证**

| 项 | 证据 |
| --- | --- |
| `cargo test` | 32 passed / 0 failed（11 个 `parse` fixture 回归 + 4 个版本样本 + 测试/宽度/临时文件） |
| `cargo fmt --check` + `clippy -D warnings` | 双绿（`make lint`） |
| macOS 双架构产物 | `stui` 824 KB (arm64) / 875 KB (x86_64)，实跑 `--version` 均输出 `stui 0.1.0` |
| `stui ls` 无 ANSI | 管道输出 ESC 字节数 **0**；字段为 Tab 分隔的固定 5 列 |
| 退出码 | 有会话 0 / 无会话 1 / screen 缺失 2（后者按代码路径保证） |
| `doctor` @ 4.00.03 | 7 pass · 4 warn · 0 fail，退出码 0；`-Q` 不可用与退出码 8 异常均如实报出 |
| `doctor` @ 4.6+（替身） | 10 pass · 1 warn · 0 fail；`-Q` 可用、hardcopy 可用两条 Pass 路径走通 |
| `doctor` @ 4.00.03 + 会话（替身） | 11 项中 `-Q`/hardcopy 走 Warn 降级路径，措辞明确「不显示陈旧内容」 |
| `package.sh` | **用真实产物**跑通：`stui-aarch64-macos.tar.gz` 390 KB + `stui-x86_64-macos.tar.gz` 416 KB + `SHA256SUMS`；`shasum -c` 校验 OK，tar 内平铺 `stui` + `INSTALL.txt`，解压后实跑 `--version` 输出 `stui 0.1.0`。**macOS 两个包当前即可下载使用** |

**未完成 / 待补**

1. **T0.1 ①②（Linux musl 产物）**：`make build-linux` 依赖 `ghcr.io/rust-cross/cargo-zigbuild:0.17.1`。
   本机 docker daemon 需手动启动（已启动），镜像 arm64 变体共 **1091 MiB**，
   实测 GHCR 单流约 **424 KB/s**，首次拉取耗时过长，本次未跑完。
   脚本与 CI 已按实测修正（原文档的 `v0.19.8` **不存在**，见 `tech-design.md` §6.2 要点 2/3）。
   补跑方式：`make build-linux && make verify-linux`。
2. **T0.6 的 9 项**：需要一台真实 Linux 服务器（沙箱内 detached 会话被立即回收）。
   按计划要求，**T0.6 必须在 T1.5 验收前完成** —— 连接语义的正确性只能以实测为准。
3. M0 期间对 `-X`/`-Q`/hardcopy 的验证用的是**脚本替身**（`$STUI_SCREEN` 注入伪造的 screen 二进制）。
   它能验证本工具的判定与降级逻辑，但**不能替代**真实语义实测，T0.6 仍开着。

### 2.2 M0 触发的需求回改（唯一一处）

**FR-19 验收 1**：原文「统计方式优先用 `screen -q -ls` 的退出码（9/10/11+），比解析文本更抗版本差异」。

实测 macOS 自带 4.00.03 在空列表时返回 **8**，该表在**最低兼容版本上就不成立**。
已改为「退出码只作快路径，不在表内即退回文本明细，矛盾时以文本为准」，
并同步修订 `requirements.md` FR-19、§7 命令映射、`screen-capabilities.md` §4.3/§5/§7、
`tech-design.md` §3.2 —— 因为「优先用退出码」这个判断会直接误导 M1 的 `refresh()` 实现。

---

## 3. M1 —— P0 核心闭环（README 4 条全覆盖）

| 任务 | 内容 | 依赖 | 验收标准 |
| --- | --- | --- | --- |
| T1.1 终端守护 | `ui::TuiGuard`（RAII：raw mode / alternate screen / 光标）+ panic hook + SIGINT/SIGTERM handler + 事件循环骨架 | T0.2 | ① 正常退出、`q`、panic、`kill -9` 四条路径后终端状态均完好（验收必测）；② `KeyEventKind::Press` 过滤生效；③ 不启用键盘增强协议 |
| T1.2 列表视图 | `ui::list` + 状态图标（○◆◈✕?）+ `util::width` 裁剪 | T1.1, T0.3 | CJK/emoji 会话名 fixture 不错行；超宽时先截名字、绝不丢序号与状态（FR-01 验收 3） |
| T1.3 双布局 | 四档布局（≥100 / 72–99 / <72 / <50）+ 小屏信息分级 | T1.2 | resize 后下次刷新自动重排；40×20 下首屏可见 ≥5 条会话；隐藏信息均有按键入口（FR-04/05） |
| T1.4 新建会话 | `n` 向导（名/目录/命令）+ 自动命名 + `screen -U -dmS` + 错误路径 | T1.2 | 重名/非法名即时报错；创建后按配置直接进入或停留列表选中新会话；失败给可行动报错（FR-02 全部验收项） |
| T1.5 连接闭环 | 状态分派连接（`Enter` 一律接管：detached `-r` / attached+multi `-d -r`；`x` 键共享 `-x`）+ detach 提示 + **`exec` 替换进程**进入 `screen`（v0.2 二修：stui 退出，detach 回原 shell） | T1.1, T0.3, T0.6 | FR-03 验收：dead 拒连（错误弹层）、全名回退、`$STY` 三处标识（页眉徽标 + 列表 `@` + 拒连自身）、会话消失不卡死、exec 前还原终端且落账先行 |

M1 出口 = requirements.md「整体验收」第 1、3 条（纯键盘 40 列全流程；detach 回原 shell —— v0.2 二修口径）。

---

## 4. M2 —— P1 增强

| 任务 | 内容 | 依赖 | 验收标准 |
| --- | --- | --- | --- |
| T2.1 配置 | `config.rs`：XDG 解析、原子写、损坏降级、`version` 迁移位 | T1.2 | tmp+rename 原子写；损坏文件备份 `.bak` 后默认值启动并提示；未知高版本只读（§8.3） |
| T2.2 元数据探测 | `screen::probe`：Linux `/proc`、macOS `proc_pidinfo`/`KERN_PROCARGS2`（libc 直调，v0.2 替代 ps/lsof） | T0.3 | 取不到返回 `None`，UI 隐藏字段，不猜测（C-5）；探测失败不影响主流程 |
| T2.3 预览 | `ui::preview` + hardcopy 0600 临时文件 + RAII 删除 | T0.2, T0.6 | 成功/失败/panic 三路径均删临时文件；不可用时明确降级不显示陈旧内容（FR-15） |
| T2.4 会话操作 | 远程断开 `D`、终止 `K`（确认框默认焦点在取消）、重命名 `r`、dead 清理 `W`（`-wipe` + 确认） | T1.5 | 每个危险操作二次确认；确认框显示会话名+命令；操作前重新校验状态（NFR-08）；attached/multi 与自身所在会话（`$STY`）拒 kill（FR-13 验收 5/6） |
| T2.5 过滤与详情 | `/` 即时过滤、`i` 详情弹层、`?` 帮助 | T1.3 | 过滤不闪屏不丢选中；`-Q` 不可用时窗口数字段隐藏而非显示 0 |
| T2.6 元数据持久化 | managed/unmanaged 模型 + 别名/描述 + 重启 managed 会话 | T2.1 | unmanaged 禁用重启；会话消失后元数据保留，手动清理入口可用（FR-24） |
| T2.7 收藏目录 | 目录收藏 + 最近使用排序 + 新建时数字键直选 | T2.1 | 上限可配（默认 10）；自定义路径自动入库（FR-23） |

M2 出口 = requirements.md「整体验收」全部 4 条。

---

## 5. M3 —— P2（按需排期，不阻塞发布）

| 候选 | 触发条件 |
| --- | --- |
| 主题（16/256 色安全色） | 有真实使用反馈后再做 |
| 会话日志（`-L`/`-X log`） | 长任务回看需求出现时 |
| 语义化标题（Linux `/proc` + agent 转录） | 明确标注 Linux-only；需要 `~/.claude/projects` 结构稳定 |
| 工作区模式（仓库树） | 多仓库日常使用成为主场景时 |
| 双语界面 | 若对外分发 |
| 鼠标支持 | 仅桌面，Screen 4.7+（SGR 1006） |

---

## 6. 横切约定（全程有效）

1. **每任务一提交**，commit message 引用任务号（如 `T0.3: session list parsing`）；单人项目直接 main，不设 feature 分支。
2. **CI 门禁**从 T0.1 起生效：`cargo fmt --check` + `clippy -D warnings` + `cargo test`。
3. **fixture 先行**：解析相关任务先写 fixture 再写实现（`-ls` 格式差异是本项目的最大风险面）。
4. **不实现任何 `stuff` 注入路径**——评审代码时如出现 `stuff`/按键注入即打回（需求 §1.3 非目标 #1）。
5. 依赖升级 = 显式变更（锁 minor + `Cargo.lock` 入库，tech-design §10 风险 1）。

## 7. 关键依赖链（关键路径）

```
T0.1 构建链 ──────────────┐
T0.2 caps ──┬─ T0.4 ls ──┼─ T0.6 服务器补测 ──┐
T0.3 parse ─┴─ T0.5 doctor│                    │
                          │                    ▼
              T1.1 终端守护 ── T1.2 列表 ──┬── T1.3 布局 ── T2.5
                                          ├── T1.4 新建      │
                                          └── T1.5 连接闭环 ──┴── T2.1 配置 ── T2.2 probe
                                                   │                    │
                                                   └── T2.3 预览         └── T2.6/T2.7
                                                   └── T2.4 会话操作
```

关键路径：**T0.3 → T1.1 → T1.2 → T1.5 → T2.x**。T0.6（服务器补测）不阻塞编码，但**必须在 T1.5 验收前完成**——连接语义的正确性以实测为准，这是唯一无法在本机验证的部分。
