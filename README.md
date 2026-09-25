# stui

一个管理 GNU Screen 会话的 TUI，为手机尺寸的 SSH 终端优化。

在 SSH（尤其是手机 SSH）里，把「记 PID、猜会话、敲一长串 `screen -r`」的操作，换成一块能看懂、能直接操作的面板。典型用途：用 Screen 兜住 CodeX / Claude Code 这类长跑 AI 任务，SSH 断了任务不死。

## 特性

- **会话列表**：序号、会话名、状态（可用 / 已占用 / 多端 / 已死 / 不可达）、PID；4.6+ 还能显示创建时间。名字超宽先截断名字，绝不牺牲序号与状态列。
- **新建会话**：默认值优先 —— 名字取当前目录名、目录取 `$PWD`、命令取 `$SHELL`，一屏落地，要改才展开。
- **一键连接**：按会话状态自动分派 `-r` / `-d -r` / `-x`，不用记参数区别。已占用（attached / multi）默认直接接管，`x` 共享连接。
- **只读预览**：`p` 通过 `hardcopy` 快照预览会话内容，绝不向会话注入按键。
- **管理动作**：detach、kill、wipe、重命名，均有二次确认；dead 会话一键清理。
- **环境自检**：`stui doctor` 逐项检查 screen 安装与配置，给出可行动的修复建议。

## 安装

暂无包管理器分发，从源码构建：

```sh
cargo build --release
```

目标平台：linux/amd64、linux/arm64（musl 静态链接）、macOS arm64 / x86_64。

## 使用

```sh
stui            # 打开 TUI
stui ls         # 纯文本会话列表（无 ANSI，可管道；非 TTY 自动切机器模式）
stui ls --full  # 追加 <pid>.<name> 寻址列
stui doctor     # 环境自检报告
```

`stui ls` 退出码：`0` 有会话 / `1` 无会话 / `2` 环境异常。

### TUI 按键

| 键 | 动作 |
| --- | --- |
| `j`/`k` 或 `↑`/`↓` | 移动 |
| `Enter` / `1`–`9` | 连接（已占用直接接管） |
| `x` | 共享连接（`-x`） |
| `p` | 只读预览 |
| `n` | 新建会话 |
| `i` | 会话详情 |
| `/` | 过滤 |
| `r` | 重命名 |
| `D` / `K` / `W` | detach / kill / wipe（二次确认） |
| `R` | 手动刷新 |
| `?` | 帮助 |
| `q` / `Esc` | 退出 / 返回 |

## 配置

配置文件位于标准位置（Linux: `~/.config/stui/config.json`；macOS: `~/Library/Application Support/stui/config.json`），可随时删除，工具不留痕。支持收藏目录（新建会话表单里 `1`–`9` 直选）、会话别名、界面语言（`zh` / `en` / `auto`）。

环境变量：

| 变量 | 说明 |
| --- | --- |
| `STUI_LANG` | 覆盖界面语言（`zh` / `en`） |
| `STUI_SCREEN` | 指定 screen 可执行文件路径（默认搜 `PATH`） |
| `STUI_AUTO_REFRESH` | 自动刷新间隔（秒，正整数）；默认纯手动刷新 |
| `SCREEN_TUI_HOME` | 覆盖配置目录 |

## 兼容性

兼容 GNU Screen 4.00.03 起（2006 年）的所有版本，包括 macOS 自带的旧版。只依赖 `-ls` / `-dmS` / `-r` / `-x` / `-d` / `-X` / `-wipe` 这套老接口；`-Q` 等新能力缺失时自动降级，不报错。

设计约束：非侵入（不向会话写输入）、不动用户环境（不改 `.screenrc`、不装服务）、取不到的信息留空，绝不编造。

## 设计文档

需求、技术方案、Screen 能力边界与参考项目分析见 [`design/`](design/) 目录：

- [`design/requirements.md`](design/requirements.md) — 开发需求文档（FR / NFR）
- [`design/tech-design.md`](design/tech-design.md) — 技术方案（Rust / ratatui 架构）
- [`design/screen-capabilities.md`](design/screen-capabilities.md) — Screen 能力边界
- [`design/reference-projects.md`](design/reference-projects.md) — 参考项目横向分析

## License

MIT
