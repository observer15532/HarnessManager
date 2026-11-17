# HarnessManager

一个只做一件事的 Windows 托盘程序：替你管住**一个** `dsh web` 进程。
启动、停止、切 profile、检查/更新 dsh，双击图标打开网页 —— 不用再开终端。

- 单文件 exe，约 750 KB，无运行时依赖
- 托盘菜单即状态板，没有主窗口
- 停止是硬杀整棵进程树（Job Object），不会留下占着端口的僵尸 MCP server

## 快速开始

```
G:\Code\Code\DeepSeekHarness\launcher\dist\HarnessManager.exe
```

双击后托盘多一个圆点图标（可能在溢出区里，拖到任务栏上即可）。左键双击 = 打开网页，
右键 = 菜单。图标颜色即状态：灰=未运行，黄=启动中，绿=运行中，蓝=维护中。

第一次使用建议：右键 → 打开配置文件，确认 `cwd` 是你的项目目录。

> 如果你在终端里手动跑着 `dsh web`，launcher 会显示“已有实例”并允许直接打开它，
> 不会去 kill 你的进程。等手边方便时退出手动实例，再用菜单的**启动**。

## 配置

`%APPDATA%\HarnessManager\config.toml`

```toml
port = 3080
channel = "next"          # next = 预发布；latest = 稳定版
start_on_launch = true    # launcher 启动时自动运行 current
current = "web"

[[profiles]]
name = "web"              # 建 profile 只需一个名字，目录由 dsh 自动建

[[profiles]]
name = "crm-m0-test"
# cwd = "G:/Code/ProjectX"   # 高级项，一般用不到，见下
```

**workspace 不由 launcher 配置。** dsh 在任何目录都能启动，工作区是在 Web UI 里
添加并选中的。`cwd` 只是进程的默认文件系统位置（也会作为 UI 里"添加工作区"的建议目录），
不写就用 `%USERPROFILE%`；想改可以直接编辑配置文件，菜单里不提供这一项。

## 菜单

| 项 | 说明 |
| --- | --- |
| 打开网页 | 优先用启动时捕获的带 token URL；双击图标同效 |
| 启动 / 停止 | 停止会整树终止，有确认需求的场景请自行留意 |
| 配置方案 ▸ | 切换、新建、删除，见下节 |
| 检查更新… | `npm view @deepseek-ai/dsh@<channel>`，只读 |
| 更新到 x.y.z | 显示 当前 → 目标，确认后停服 → 安装 → 校验 → 失败回滚 |
| 打开配置文件 / 日志目录 / DSH 目录 | 字面意思 |
| 开机自启：已开启 / 未开启 | 文字即状态，不用猜；写注册表后会回读校验 |

日志：`%APPDATA%\HarnessManager\logs\`
- `launcher.log`：菜单点击、状态迁移、启动时的 autostart 状态
- `dsh-<profile>-<时间>.log`：dsh 的 stdout/stderr（token 已打码）

## 配置方案管理

profile 回答的是"这套配置由哪些插件按什么顺序组成"。它在 `$DSH_HOME\profiles\<name>`，
**只装插件与配置，不含数据**——工作区、会话、凭据都在 DSH_HOME 层级，被所有 profile 共享。

菜单「配置方案 ▸」：

- **新建…**：只需输入方案名（字母/数字/`-`/`_`，`desktop` 保留给 Electron 版）。
  会立刻用 `--from-default-profile web` 在 `$DSH_HOME\profiles\<name>` 初始化 profile，
  等价于跑一次 `--dump-config`，**不会启动服务**；初始化成功后才写入配置，失败则什么也不留。
- **删除当前方案…**：确认后**连 `$DSH_HOME/profiles/<name>` 一起删除**（该方案装的插件和配置
  都在里面，不可恢复）。内置方案（如 `web`）下次启动会自动重建一个干净的。

保存配置用 `toml_edit` 就地编辑，手写的注释和键顺序都会保留。

## 命令行

```
HarnessManager.exe profile list
HarnessManager.exe profile add <name> --cwd <dir>
HarnessManager.exe profile remove <name>

HarnessManager.exe --autostart status   # enabled / disabled
HarnessManager.exe --autostart on
HarnessManager.exe --autostart off
```

在 PowerShell 里请用 `cmd /c` 包一层：PowerShell 不会等待 GUI 子系统程序，
输出会乱序甚至丢失。

```
cmd /c "dist\HarnessManager.exe" --autostart status
```


## 设计要点

- **启动命令**是 `<node> <dsh>/lib/bin.js --profile X --port N --no-open`。
  `--no-open` 把“打开网页”的时机交给托盘；直接跑 `bin.js` 绕开 `dsh.cmd` shim。
- **进程树**挂在 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 job 上，
  所以 launcher 崩了 dsh 也会跟着走，端口不会残留。
  代价是硬杀：正在跑的回合会中断。
- **外部实例**（端口上有东西但不是自己的子进程）只提供“打开”，绝不终止。
- 更新失败会自动 `npm install -g @deepseek-ai/dsh@<旧版本>` 回滚。

## 已知限制 / 下一步

- 托盘图标是运行时画的圆点，exe 本身没有 ICO 资源（资源管理器里是默认图标）
- 没有系统通知：结果写在菜单第一行，需要决策时用 MessageBox
- `更新 dsh` 的实机演练尚未做完整
- Node 不做代管：**每次启动前**用已安装 dsh 的 `engines.node` 校验，不满足就拒绝启动并提示，
  永远不会自动升级（升级会影响其它全局工具，并让 profile 里的原生模块失效）
- 更新 dsh 前同样会用目标版本的 `engines.node` 做前置拦截，不满足直接拒绝

## 开发

```
cargo build --release          # 产物 target\release\HarnessManager.exe
powershell -File dev\smoke.ps1 # 隔离环境端到端测试（临时 APPDATA/DSH_HOME，端口 3099）
```

`dev\smoke.ps1` 会验证：单实例、进程启动、`dsh web:` URL 解析、
以及**终止 launcher 后进程树是否干净退出**。

