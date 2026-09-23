# 测试 fixture

`-ls` 解析是本项目最大的风险面（版本/`sort`/CRLF 都会改输出），所以每个样本都标注来源。
**「实测」= 在本机真跑抓下来的原文；「推导」= 依据手册或社区格式构造，尚未在对应版本上核对。**
未经实测的样本在注释里一律标出，避免日后把它们当成已验证事实。

| 文件 | 来源 | 用途 |
| --- | --- | --- |
| `ls-400c03-single.txt` | **实测** macOS 自带 Screen 4.00.03（2026-09-23） | 无日期列格式的回归；socket 目录从尾行提取 |
| `ls-46-with-date.txt` | **推导**（手册/社区格式），行序刻意打乱 | 含创建时间列；验证解析后按名稳定排序 |
| `ls-empty.txt` | **实测** 4.00.03 无会话 | `No Sockets found in <dir>.` 骨架 |
| `ls-dead-unreachable.txt` | **推导**（手册状态词语义） | dead/unreachable/multi 的归类与「禁连」判定 |
| `ls-malformed.txt` | 人工构造 | 空行、非数字 pid、空名、未知状态、超长名、重名、只有日期无状态、无括号行 |
| `ls-cjk-emoji.txt` | 人工构造（T1.2） | 渲染层宽度测试：CJK / emoji / 超长名 / 四种状态；不新增解析语义 |
| `ls-usage-dump.txt` | **实测** `screen -Q windows` 在 4.00.03 上的完整输出 | 必须返回 `Err`，不能被误解析成会话 |
| `version-400c03.txt` | **实测**（保留原始 CRLF 行尾） | `Screen version 4.00.03 (FAU) 23-Oct-06` → `(4,0,3)` |
| `version-40602.txt` | 推导 | `(4,6,2)` |
| `version-502.txt` | 推导 | `(5,0,2)` |
| `version-nonstandard.txt` | 构造 | 非标准输出必须返回 `None`（不 panic、不猜） |

## 关于最后一项

`ls-usage-dump.txt` 是实测抓到的 usage dump。它里面有这些行：

```
-d (-r)       Detach the elsewhere running screen (and reattach here).
-D (-r)       Detach and logout remote (and reattach here).
-t title      Set title. (window's name).
```

如果只按「含括号 = 会话明细」来判断，这些选项说明会被当成会话。
因此解析器用「首个点号前的分量必须是纯数字 pid」作为准入判据 —— 这条与版本无关，
且能把 usage dump 干净地挡在门外。这个 fixture 就是这条规则的守门测试。

## 待 T0.6 补测后才能确认的样本

- `ls-46-with-date.txt`：日期列的**精确格式**（`08/09/2026 10:23:45 AM`）按社区写法构造。
- `ls-dead-unreachable.txt`：`(Dead ???)` 与 `(Multi, attached)` 是真实 screen 的常见写法，
  但本机无 4.6+ 也无 dead 会话，**未实测**。解析器对它们做的是「取第一个词、去尾部逗号」的宽松归类。
- CRC 换行：实测 4.00.03 的行尾是 `\r\n`，fixture 文件本身用 `\n` 保存，
  由 `parse::tests::tolerates_crlf_line_endings` 在测试里替换成 CRLF 来覆盖，避免依赖编辑器行为。
