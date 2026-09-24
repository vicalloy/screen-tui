# Screen TUI 技术方案（Rust）

- 版本：v1.0（决策已确认，见下表与 `requirements.md` §14）
- 前置文档：`requirements.md`（需求）、`screen-capabilities.md`（Screen 能力边界）、`reference-projects.md`（参考项目）

---

## 1. 已确认决策

| 决策项 | 结论 |
| --- | --- |
| 技术栈 | Rust 单二进制 |
| TUI 框架 | ratatui + crossterm |
| 并发模型 | 无 async，std 线程 + 事件超时轮询 |
| 连接方式 | 让出终端，前台 exec `screen`，detach 后自动回 TUI |
| Screen 兼容下限 | 4.00.03（无 `-Q`，能力探测 + 降级） |
| 目标平台 | linux/amd64、linux/arm64（musl 静态，Docker 编译）；macOS arm64/x86_64（本机编译） |
| 分发 | 仅 GitHub Releases 二进制下载，不做安装脚本 |
| 界面语言 | 英文默认；`zh`/`en` 双语（i18n 手写翻译表），`STUI_LANG` > 配置 `language` > locale 探测 |

---

## 2. 总体架构

单 crate 二进制，按「screen 适配层 / 应用状态机 / 渲染层 / 外围」分层。核心原则：

1. **解析与渲染严格分离** —— `screen::parse` 是纯函数集合，不碰终端，全部可单测（NFR-10）。
2. **能力对象贯穿全局** —— 启动时探测一次 `screen -v`，得到 `Caps`，此后所有功能开关只读 `Caps` 字段，代码里不出现版本号字面量比较。
3. **Screen 是唯一事实来源** —— 会话状态每次操作前重新查询（NFR-08），本工具不缓存「真相」，只缓存展示层。

```
┌────────────────────────────────────────────────┐
│ main.rs / cli.rs      clap 参数 → 子命令分发    │
├────────────────────────────────────────────────┤
│ app.rs                状态机（Mode 枚举驱动）    │
│   ├── ui/             ratatui 渲染（无业务逻辑）│
│   ├── screen/         Screen 适配层             │
│   │    ├── caps.rs    版本探测 → Caps           │
│   │    ├── parse.rs   -ls 解析（纯函数）        │
│   │    ├── cmd.rs     命令构造与执行            │
│   │    └── probe.rs   /proc、ps 元数据探测      │
│   ├── config.rs       配置读写（原子写）         │
│   ├── doctor.rs       环境自检                  │
│   └── util/           宽度裁剪、临时文件         │
└────────────────────────────────────────────────┘
```

### 2.1 状态机

```
enum Mode {
    List,                      // 主列表（默认轮询）
    Filter,                    // / 输入过滤词
    NewSession(NewDraft),      // n 新建（单表单：Tab/↑↓ 切字段，Enter 创建，Esc 取消）
    Rename,                    // r 重命名
    Confirm(ConfirmAction),    // kill / wipe / detach 的二次确认
    AttachChoice,              // attached 会话的 共享/接管/取消
    Help,                      // ? 帮助弹层
    Detail,                    // i 详情弹层（窄屏）
    Preview,                   // p 预览视图
}
```

`Mode` 之间只通过显式事件转换；`Esc` 统一回退上一层，`q` 在 `List` 才退出。渲染层只读 `App`，不改变它 —— 保证任何绘制路径都不能引入副作用。

`NewSession` 是单层表单，没有「上一步」：三个字段靠 `Tab`/`↓`/`↑` 循环切换焦点，`Esc` 在任意字段直接取消整个向导回列表 —— 即 FR-02 验收 6 的口径。

### 2.2 事件循环（无 async）

crossterm 的事件轮询自带超时，一个线程就够，不需要后台刷新线程：

```rust
loop {
    terminal.draw(|f| ui::render(f, &app))?;

    let timeout = app.next_tick_in().min(POLL_CAP);  // 封顶 200ms：信号最长延迟 200ms 可见
    if crossterm::event::poll(timeout)? {
        match crossterm::event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => app.on_key(k),
            Event::Resize(_, _) => { /* draw 自动重排 */ }
            _ => {}
        }
    }
    if app.tick_due() {
        app.refresh();                          // 仅在 $STUI_AUTO_REFRESH 开启时触发（FR-19 v0.2）
    }
    app.ensure_window_count(detail_visible);    // 窗口数懒获取，10s TTL（FR-17 v0.2）
}
```

要点：

1. `KeyEventKind::Press` 过滤必须有 —— Windows/部分终端会上报 Release 事件，不滤会导致按键双触发。
2. **不启用 crossterm 的键盘增强协议**（kitty protocol），保持与老 SSH 客户端（含手机客户端）最大兼容。
3. attach 期间事件循环整个让位（见 §3.1），不存在「后台仍读事件」的竞态。
4. *（v0.2）* 刷新默认**纯手动**（`R` + 动作后路径）：`refresh_interval == None` 时 `next_tick_in` 返回
   `MAX`（被 `POLL_CAP` 钳制，纯阻塞等待），`tick_due` 恒否 —— 空转 CPU 实测 0%。

### 2.3 与需求文档的能力映射

`Caps` 结构即 `screen-capabilities.md` §5 能力矩阵的代码化：

```rust
enum Support { Yes, No, Unknown }   // Unknown = 探测条件不具备，≠ 不支持

struct Caps {
    version: Option<(u32, u32, u32)>,  // 4.00.03 → (4, 0, 3)；"4.06.02" → (4, 6, 2)
    query: Support,                    // -Q 可用（4.6+）：windows/title/number/info
    hardcopy: Support,                 // -X hardcopy
    hardcopy_history: Support,         // -X hardcopy -h
    mouse_sgr: bool,                   // SGR 1006（4.7+，仅桌面可选启用）
}
```

**实现要点（M0 定稿）**

1. 能力字段用**三态**而非 `bool`：`-Q` 只有在「有会话可试跑」时才能判定；没有会话时是
   `Unknown`（不知道），**不等于** `No`（不支持）。用 `bool` 会把「没测过」静默当成「不支持」，
   与 C-5「不猜测」冲突。
2. 探测顺序：`screen -v` 解析版本（**只看文本，成功也返回退出码 1**）→ 对有会话的
   `-Q windows` 试跑确认 `query`（版本号解析可能骗人，实测为准）。
3. `hardcopy` 探测**不放进启动路径**：它需要为一个真实会话落盘一次，
   而 §5 又写明「doctor 是唯一有副作用的检查」。因此 `Caps::detect()` 把它留成 `Unknown`，
   由 `doctor`（以及后续预览功能首次使用时）显式调用后回填。
4. `mouse_sgr` **恒为 `false`**：SGR 1006 需要 4.7+，但按 §2 原则 2 不允许做版本号比较，
   而鼠标没有外部探测手段。宁可不启用（FR-37 本身也只是桌面可选项），也不伪造一个「猜它支持」的结论。
5. `escape_prefix` 在 M0 恒为默认值，`.screenrc` 探测属 FR-18（M2）。

## 3. 核心机制

### 3.1 连接：让出终端（FR-03 的实现契约）

这是全工具最关键的交互，必须保证 raw mode / 备用屏幕缓冲区 / 光标状态在**所有**路径下还原：

```rust
fn attach(app: &mut App, s: &SessionRef, mode: AttachMode) -> Result<i32> {
    // 1. 重新校验会话仍在且状态匹配（NFR-08）
    let st = screen::status_of(s)?;
    ensure_attachable(&st, mode)?;          // dead/unreachable 直接拒绝

    // 2. 让出终端：禁 raw mode、离开 alternate screen、显示光标
    let mut tui = TuiGuard::enter(app.terminal)?;   // RAII
    tui.suspend()?;

    // 3. 返回提示（探测到的转义前缀；探测不到则注明"若改过前缀请用前缀+d"）
    print_detach_hint(app.caps.escape_prefix);

    // 4. 前台执行 screen，继承当前 tty，阻塞至用户 detach
    let code = match mode {
        AttachMode::Takeover => screen::cmd(&["-r",  &s.full])   // 或 -d -r
        AttachMode::Share    => screen::cmd(&["-x",  &s.full]),
    }?;

    // 5. 恢复 TUI：重进 alternate screen + raw mode，强制全量重绘
    tui.resume()?;
    app.refresh();
    Ok(code)
}
```

**RAII 守护 `TuiGuard`**：`Drop` 里无条件还原终端；另设 `std::panic::set_hook`，panic 时先还原终端再走默认 hook（打印 panic 信息），保证崩溃后终端不报废。`SIGINT`/`SIGTERM` 注册 handler 走正常退出路径。进入备用屏幕（`enter`/`resume`）后先 `Clear(All)` 再首帧：ratatui 是 diff 渲染，不清屏的话备用屏幕上残留的旧内容（shell 输出 / 上次异常退出的画面）不会被覆盖。

**尺寸问题**：接「共享连接」时提示「另一端窗口可能被压小」；「接管」后若尺寸异常，给出「按 `D` 断开对端再连」的提示（`-A` 自适应作为接管路径的默认参数）。

### 3.2 会话枚举与解析（FR-01）

两级策略，把版本差异的影响面压到最小：

```rust
fn enumerate() -> Enumeration {
    // 第一级：screen -q -ls 拿退出码 —— **只作快路径**
    //   9=无会话 / 10=不可连 / 11+n=可用数  → 命中即可采信
    //   其他值（4.00.03 空列表实测返回 8）  → 不可判定，必须走第二级
    // 第二级：screen -ls 解析明细 —— **必经路径**，不是可选优化
    //   解析失败不 panic，返回 Err 交给上层降级展示
    // 两者矛盾时以文本明细为准
}
```

> M0 实测修订：原文档假设 `-q -ls` 退出码「版本无关」。实测 4.00.03 空列表返回 **8**（非手册的 9），
> 该表在最低兼容版本上不成立 —— 详见 `screen-capabilities.md` §4.3。因此第二级解析器必须自己够健壮。

解析器规则（对应 `screen-capabilities.md` §4）：

1. 行正则：`^\t(?P<full>\S+)\t(?P<date>\(\d{2}/\d{2}/\d{4}[^)]*\))?\t?\((?P<state>[^)]+)\)$` —— **日期组可选**。
2. `full` 拆为 `pid` + `name`（第一个 `.` 分割）；状态词做白名单映射，未知状态原样透出并标记为未知（不猜）。
3. 表头单复数（`There is a screen on:` / `There are screens on:`）、尾行 `N Socket(s) in <dir>.`、空列表 `No Sockets found ...` 三种骨架都识别；**socket 目录从尾行提取，绝不硬编码**。
4. `.screenrc` 的 `sort` 会改行序 —— 解析后按名称稳定排序，展示顺序不依赖 screen。
5. 回归样本至少 4 组：4.00.03 无日期 / 4.6+ 有日期 / 空列表 / 含 dead+unreachable，作为 `cargo test` 固定 fixture。

**实测补充的三条硬约束**（M0 实现时测出，已进 `parse` 的 fixture 与单测）：

6. **读 stdout，不是 stderr**；解析前逐行 strip `\r`（实测行尾是 `\r\n`，不去掉则状态词匹配全失败）。
7. **准入判据用「首个点号前是纯数字 pid」**：`-X` 报错时会把完整 usage dump 到 stdout，里面含
   `-d (-r) ...`、`-t title. (window's name).` 这类带括号的行 —— 只按「含括号」判断会把选项说明当成会话。
   该规则与版本无关，且由 `ls-usage-dump.txt` fixture 守门。
8. 无法识别的行**不丢弃也不静默**：计入 `warnings` 一并上报；整段输出完全无法识别时才返回 `Err`。

### 3.3 预览（FR-15）

```rust
fn preview(s: &SessionRef, caps: &Caps) -> Result<String> {
    let path = tmpfile_0600()?;                       // $TMPDIR/stui-preview-<pid>
    let r = screen::cmd(&["-S", &s.full, "-X", "hardcopy", path]);
    let out = match r {                               // 读后立刻删，Err 也要删
        Ok(_) => fs::read_to_string(&path).map(trim_and_clamp),
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(&path);
    out.map_err(|_| PreviewUnavailable)               // 明确降级，不显示陈旧内容
}
```

1. 临时文件权限 `0600`（`OpenOptions::mode(0o600)`），路径在 `$TMPDIR`，**成功/失败/panic 三条路径都删**（放进同一个 RAII guard）。
2. 每次预览都重新抓取，**不缓存**；预览视图标注抓取时间，避免误当实时。
3. `-h`（含回滚）仅当 `caps.hardcopy_history` 探测通过才提供。
4. hardcopy 由会话进程写出，写入方身份是会话属主 —— `doctor` 预检里包含一次真实试写（§5）。

### 3.4 元数据探测（工作目录 / 运行命令）

Screen 不提供这些信息（capability 文档 §8 的边界结论），按平台分派：

| 平台 | 实现 |
| --- | --- |
| Linux | 会话 pid → `/proc/<pid>/cwd`（readlink）、进程树首个非 screen 子进程的 `/proc/<pid>/cmdline` |
| macOS | libc 直调（v0.2，替代 spawn `ps`/`lsof`，单次从 ~50ms 降到 ~30µs）：`proc_listchildpids` 定位子进程，`proc_pidinfo(BSDINFO)` 拿 comm、`sysctl(KERN_PROCARGS2)` 拿完整 argv，`proc_pidinfo(VNODEPATHINFO)` 拿 cwd。libc crate 缺常量/结构的部分按 `proc_info.h` 手写，布局经实测锚定并有单测自检（`proc_cwd_matches_current_dir` 等） |

统一抽象为 `probe::session_meta(pid) -> Option<Meta>`；**任何一步取不到就返回 `None`，UI 隐藏该字段**（C-5 不猜测）。探测失败不影响列表与连接主流程。

### 3.5 配置（FR-23/24）

- 位置：`$SCREEN_TUI_HOME` > `$XDG_CONFIG_HOME/screen-tui/` > `~/.config/screen-tui/`；结构沿用 `requirements.md` §8.2。
- **原子写**：同目录写 `config.json.tmp` → `fsync` → `rename`；目录 `0700`、文件 `0600`。
- 损坏容错：解析失败 → 备份为 `config.json.bak` → 用默认值启动并在 UI 提示（`requirements.md` §8.3）。
- 自绘 XDG 解析（约 15 行），不引入 `dirs` crate。

---

## 4. 依赖清单

| crate | 版本策略 | 用途 | 说明 |
| --- | --- | --- | --- |
| `ratatui` | 锁定 minor | TUI 渲染/布局 | 纯 Rust |
| `crossterm` | 锁定 minor | raw mode / 事件 / alternate screen | ratatui 官方搭配；**不启用增强键盘协议** |
| `clap`（derive） | 4.x | CLI：`stui` / `stui ls` / `stui doctor` | |
| `serde` + `serde_json`（derive） | 1.x | 配置与状态文件 | |
| `thiserror` | 1.x | 错误类型定义 | |
| `unicode-width` | 0.2 | 显示宽度裁剪（CJK/emoji） | ratatui 传递依赖，显式声明使用 |
| `libc` | 0.3 | 仅兜底（信号注册等 crossterm 未覆盖处） | 能不用就不用 |

**明确不引入**：`tokio`（无 async 决策）、`anyhow`（`thiserror` 足够，错误要类型化降级）、`dirs`（自绘）、`tempfile`（需要精确控制 0600 与删除时机，自绘 20 行）。

发布配置：

```toml
[profile.release]
strip = true
lto = "thin"
codegen-units = 1
```

`Cargo.lock` 入库 —— 二进制分发模式下可复现构建是底线。

---

## 5. CLI 形态与 doctor

```
stui                    # TUI（默认）
stui ls                 # 纯文本会话列表（无 ANSI，可管道）
stui doctor             # 环境自检（requirements.md §10 的 11 项）
stui --version
```

退出码约定：`0` 正常 / `1` 业务失败（如无会话）/ `2` 环境异常（screen 缺失等）—— 与 FR-22 一致。

`doctor` 检查项沿用 `requirements.md` §10，实现上是 `Vec<Check>`，每项产出 `Pass | Warn | Fail(reason, fix_hint)`；`Fail` 任意一项存在则退出码 2。其中「预览能力」一项会真实执行一次 `hardcopy` 试写，因此 `doctor` 是唯一有副作用的检查，需在输出中注明。

---

## 6. Docker 跨平台编译

### 6.1 目标矩阵

| Target | 工具链 | 产物特征 |
| --- | --- | --- |
| `x86_64-unknown-linux-musl` | Docker + cargo-zigbuild | 静态链接，拷贝即用 |
| `aarch64-unknown-linux-musl` | Docker + cargo-zigbuild | 静态链接，拷贝即用 |
| `aarch64-apple-darwin` | 开发机原生 `cargo build` | 动态链 libSystem（macOS 必然） |
| `x86_64-apple-darwin` | 开发机 `cargo build --target`（rustup 装 std） | 同上 |

选 musl 的理由：零 glibc 版本耦合，老发行版/Alpine/最小容器里都能直接跑 —— 与「服务器上拷贝即用」的目标一致。

选 [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) 的理由：zig 充当 linker，**在 amd64 宿主机上原生速度编译 arm64 目标，不需要 QEMU 模拟**；官方镜像自带全部组件。

### 6.2 构建脚本

镜像锁定版本，`cargo` 缓存放命名卷（跨次构建复用 registry 与 target）：

```makefile
IMG     := ghcr.io/rust-cross/cargo-zigbuild:0.17.1
TARGETS := x86_64-unknown-linux-musl aarch64-unknown-linux-musl

.PHONY: build-linux build-macos release

build-linux:
	docker run --rm -v $(CURDIR):/work -w /work \
	  -v stui-cargo:/usr/local/cargo -v stui-target:/work/target \
	  $(IMG) bash -c 'for t in $(TARGETS); do cargo zigbuild --release --target $$t; done'
	$(MAKE) extract-linux          # 把卷里的产物提取到 dist/

build-macos:
	cargo build --release --target aarch64-apple-darwin
	cargo build --release --target x86_64-apple-darwin

release: build-linux build-macos
	scripts/package.sh   # 归档 + SHA256SUMS，见 §7
```

要点：

1. `-w /work` 挂载源码，`stui-cargo` 卷持久化下载缓存，`stui-target` 卷持久化编译产物 —— 二次构建只重编改动部分。
2. **镜像 tag 修正**：原文档写 `v0.19.8`，实测 `docker pull` 报 `not found`。
   GHCR 上的 tag **不带 `v` 前缀**，最高稳定版为 `0.17.1`（`Makefile` 与两个 workflow 已同步）。
3. **产物位置修正**：原文档说「产物直接落在宿主机 `target/<triple>/release/`，无需从容器拷贝」，
   但同一段又把 `stui-target` 命名卷挂在 `/work/target` 上 —— 命名卷不是宿主机目录，产物会留在卷里；
   更麻烦的是容器以 root 运行，直接写宿主机 `target/` 会留下 root 属主文件，让后续本机 `cargo build` 撞权限错误。
   改为**两步**：容器内编译进卷 → 用 alpine 容器把产物 `cp` 到宿主机 `dist/stui-<triple>`（`make extract-linux`）。
   `dist/` 只由本流水线写入，并用 `make clean-linux`（同样是容器）清理 root 属主文件。
4. `cargo zigbuild` 传 `--target x86_64-unknown-linux-musl` 即静态链接，不需要额外 `RUSTFLAGS`；
   可用 `readelf -d | grep -c NEEDED == 0` 断言（`make verify-linux` 已内置）。
5. 若不想依赖第三方镜像，备选方案是 `rust:alpine` 自装 `musl-tools` —— 但那样 **aarch64 需 QEMU 或另一台 arm 宿主机**，所以默认走 zigbuild。

### 6.3 CI（GitHub Actions）

```yaml
jobs:
  linux:
    runs-on: ubuntu-latest
    container: ghcr.io/rust-cross/cargo-zigbuild:0.17.1
    strategy:
      matrix:
        target: [x86_64-unknown-linux-musl, aarch64-unknown-linux-musl]
    steps:
      - uses: actions/checkout@v4
      - run: cargo zigbuild --release --target ${{ matrix.target }}
      - uses: actions/upload-artifact@v4
        with: { name: ${{ matrix.target }}, path: target/*/release/stui }
  macos:
    runs-on: macos-latest          # arm64 原生
    steps:
      - uses: actions/checkout@v4
      - run: cargo build --release --target aarch64-apple-darwin
      - run: rustup target add x86_64-apple-darwin
      - run: cargo build --release --target x86_64-apple-darwin
      - uses: actions/upload-artifact@v4
        with: { name: macos, path: target/*/release/stui }
```

测试 job（`cargo fmt --check` / `clippy -D warnings` / `cargo test`）跑在 linux 原生即可，与发布 job 并行。打 `v*` tag 时由 release workflow 汇总 artifact 上传 Release（`pouch` 的 tag → draft → publish 两段式可参考，但按「仅二进制下载」简化为单段自动发布）。

---

## 7. 发布物

```
screen-tui-v0.1.0/
├── stui-x86_64-linux.tar.gz          # 内含单个静态 stui + README 片段
├── stui-aarch64-linux.tar.gz
├── stui-aarch64-macos.tar.gz
├── stui-x86_64-macos.tar.gz
└── SHA256SUMS
```

- tar.gz 内路径平铺：解压得 `stui` 一个文件，`mv` 进 PATH 即完成安装；卸载 = 删该文件 + 删 `~/.config/screen-tui/`。
- 不提供安装脚本（已确认决策）；README 写清「下载哪个包、放到哪」四步说明。
- 包名带 `-linux`/`-macos` 与架构，不暴露 triple 术语（手机上下载的人不需要知道 musl 是什么）。

---

## 8. 测试策略

| 层 | 手段 | 覆盖 |
| --- | --- | --- |
| 解析层 | `cargo test` + fixture 样本（§3.2 的 4 组，外加畸形输入：空行/未知状态/超长名/重名） | `screen::parse` 全覆盖，这是版本兼容的主战场 |
| 能力探测 | `caps.rs` 用注入的 `screen -v` 输出样本测解析（`4.00.03` / `4.06.02` / `5.0.2` / 非标准输出） | 版本 → Caps 映射 |
| 配置层 | 单测：原子写中断模拟（tmp 残留）、损坏文件降级、未知高版本只读 | `config.rs` |
| UI 逻辑 | `App` 状态机与 `Mode` 转换单测（不渲染，纯逻辑） | 按键表全路径 |
| 冒烟（可选） | Docker 内装 screen 的 debian 镜像 + `script -q -c` 造伪 tty，跑 `stui ls` / `doctor` | 真实 screen 二进制路径 |
| 实机 | `screen-capabilities.md` 附录 B 的 10 项待补测，在真实 4.x 服务器执行一次并回填 | `hardcopy`/`-X` 语义、`-q -ls` 退出码 |

`clippy -D warnings` 与 `rustfmt` 为 CI 门禁。

---

## 9. 实施顺序（对应 requirements.md §13 里程碑）

| 阶段 | 交付 | 验证方式 |
| --- | --- | --- |
| M0 | Cargo 工程 + `caps` + `parse` + `doctor` + `ls` 子命令 + Makefile/Docker 构建 | fixture 全绿；`docker run` 出双架构二进制；真实服务器补测回填 |
| M1 | TUI 骨架（List/Help）+ 新建 + 连接（含 RAII 终端守护） | 手机尺寸模拟（`tput` 改窗）走通「看→选→进→出」 |
| M2 | 预览 / 共享与接管 / 远程断开 / 终止 / 重命名 / 过滤 / 详情 / 元数据 | 危险操作二次确认全走查；预览降级路径验证 |
| M3 | P2 项按需排期（主题/日志/语义标题/工作区） | — |

M0 就把 Docker 构建链跑通 —— 构建链是最容易在最后时刻爆炸的环节，先趟平。

---

## 10. 风险与缓解

| # | 风险 | 影响 | 缓解 |
| --- | --- | --- | --- |
| 1 | ratatui/crossterm 迭代快、API 变动 | 升级成本 | 锁定 minor + `Cargo.lock` 入库；升级视为显式变更 |
| 2 | 4.00.03 上 `hardcopy`/`-X` 语义未实测（沙箱限制） | 预览/管理功能可能需调整 | M0 阶段真实服务器补测（附录 B 清单）；`caps` 探测以实跑为准，不信版本号 |
| 3 | musl 下 `crossterm`/依赖的 libc 调用 | 编译或运行失败 | ratatui+crossterm 已被 musl 目标广泛验证；M0 即验证 musl 产物在 Alpine 容器可运行 |
| 4 | 终端状态还原不彻底（panic/被 kill） | 用户终端报废，体验灾难 | RAII + panic hook + SIG handler 三重兜底；M1 验收必测 `kill -9` 后终端可用 |
| 5 | 手机 SSH 客户端键盘差异（Esc 缺失、无 Ctrl） | 部分操作不可达 | 按键表以单字符为核心；Esc 功能均提供等价键（`q` 回退）；不依赖任何 Ctrl 组合 |
| 6 | emoji/CJK 宽度错位 | 窄屏布局破坏 | `unicode-width` 全链路钳制；fixture 含 CJK 会话名样例 |
| 7 | zigbuild 镜像版本漂移 | 构建不可复现 | 镜像 tag 锁死（`0.17.1`，GHCR 上**不带 v 前缀**），升级走显式 PR |
| 8 | docker 拉取镜像慢（实测 GHCR 单流约 424 KB/s，arm64 镜像 1091 MiB） | 首次构建耗时长 | 命名卷持久化 `/usr/local/cargo` 与 `/work/target`；镜像只需成功拉取一次 |
| 9 | 本机 docker daemon 未启动（macOS 上的 Docker Desktop） | `make build-linux` 直接报 `Cannot connect to the Docker daemon` | 只有 linux 产物需要 docker；`make test` 与 `make build-macos` 均不依赖它 |

---

## 11. 目录结构（终态）

```
screen-tui/
├── Cargo.toml            # [package] name = "screen-tui"，bin 名 stui
├── Cargo.lock            # 入库
├── Makefile              # build-linux / extract-linux / verify-linux / build-macos / package / test / lint
├── scripts/package.sh    # 归档 + SHA256SUMS
├── tests/fixtures/       # -ls 样本、screen -v 样本、畸形输入（含 README 标注每个样本的来源：实测/推导）
├── .github/workflows/    # ci.yml（fmt/clippy/test）+ release.yml（tag 触发）
├── dist/                 # 构建产物落地（root 属主，.gitignore 忽略，clean-linux 清理）
├── design/               # 本目录五份文档
└── src/
    ├── main.rs           # ✓ M0
    ├── cli.rs            # ✓ M0（ls / doctor）
    ├── config.rs         # ✓ M0 仅目录解析 + 可写探针；完整读写属 T2.1
    ├── doctor.rs         # ✓ M0
    ├── app.rs            # M1
    ├── ui/{mod,list,detail,preview,dialogs,theme}.rs   # M1–M2
    ├── screen/{mod,caps,parse,cmd}.rs  # ✓ M0
    ├── screen/probe.rs   # T2.2
    └── util/{mod,width,tmpfile}.rs     # ✓ M0
```

