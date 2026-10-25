# 用户会话实例计划（`systema-sysu`, P1）

状态：**已定稿**（2026-10-10）。本文档是 P1 的实施依据；实现阶段按此执行，偏离需先更新本文档。

## 目标

在用户态提供一个与 SysI 地位对等的启动器 **`systema-sysu`（System U, User）**：
普通用户登录后可启动一个**每用户的服务管理器实例**（sysa + syss + syst + sysw），
用自己的 CLI（`--user`）和会话总线上的真实 `systemctl --user` 管理自己的单元，
登出后实例停止。系统域实例（sysi 起的那套）完全不受影响。

## 1. 语义澄清：systemd 的 `--user` 并不跨域

设计前先对齐事实（已查证 systemd 文档/ArchWiki）：

- `systemctl --user` **只**连接调用用户的用户管理器，只管理用户域单元；
  用户单元不能引用/依赖系统单元或其他用户的单元。systemd 的用户域与系统域
  分离程度与 systema 相同。
- 用户会话控制系统服务的真实通道是另外两条：
  1. 不带 `--user` 的 `systemctl` → 系统总线 → polkit 按操作授权；
  2. `sudo`/`doas` 提权。
- 反向（root 管理用户域）：`systemctl --user -M <user>@`。

**结论**：systema 缺的不是路由，是授权机制。P1 不引入 polkit（FreeBSD /
OpenBSD / Termux 无此物），保持严格分离。

## 2. 决策记录

| 议题 | 决策 |
|------|------|
| 兼容性 | 原生 CLI `--user` 优先；**session-bus `sysw` 桥提前进 P1**（真实 `systemctl --user` 直接可用，2026-10-10 由"推后到 P2+"修订，见备注 11） |
| 进程模型 | **直接复用**现有二进制（env 参数化的子集），不抽公共库 |
| 生命周期 | 会话触发启动 + 登出停止：**P1 登录钩子，P2 PAM 模块** |
| P1 单元范围 | 仅 **service + target**（worker 子集 = sysa + syss + syst + sysw；sysw 是 D-Bus 桥、不承载单元类型） |
| 启动器形态 | **独立新二进制** `systema-sysu`（不在 sysi 里加开关） |
| 环境矩阵 | 由启动器**运行时计算**注入子进程；`build.rs` 编译期默认值不动 |
| 跨域控制（用户会话控系统服务） | **方案 A：维持严格分离 + sudo**。CLI 在权限不足时给出明确的"请加 sudo"提示。细粒度 ACL（进程内策略引擎）与 polkit 桥均为后续阶段，非 P1 |
| CLI 默认域 | 无 `--user` = 系统域（与 `systemctl` 一致）；`--user` 显式选择用户域 |

## 3. 交付物 A：`systema-sysu` 启动器

新 crate：`crates/backend/systema-sysu`，加入 workspace `members`。
二进制名 `systema-sysu`，SysI 的用户态对应物。遵循既有约定：
`#[tokio::main(flavor = "current_thread")]`、`paths::init()` + `l10n::init()`、
`--version` / `--full-version` / `paths::debug_dump()`、用户可见字符串走 `t_()`。

### 3.1 环境变量矩阵（启动器计算并注入子进程）

socket 默认值是**编译期常量**（`/run/systema/...`），用户实例必须显式覆盖，
否则会碰系统域的 socket（EACCES 或互踩）。矩阵：

| 变量 | 用户域取值 |
|------|-----------|
| `SYSTEMA_RUNSTATEDIR` | `${XDG_RUNTIME_DIR}/systema`（无 `XDG_RUNTIME_DIR` 时退化为 `/tmp/systema-<uid>`） |
| `SYSTEMA_IPC_SOCKET` | `$SYSTEMA_RUNSTATEDIR/allocator.sock` |
| `SYSTEMA_CONTROL_SOCKET` | `$SYSTEMA_RUNSTATEDIR/control.socket` |
| `SYSTEMA_FDPASS_SOCK` | `$SYSTEMA_RUNSTATEDIR/fdpass.sock` |
| `SYSTEMA_NOTIFY_DIR` | `$SYSTEMA_RUNSTATEDIR/systema/notify`（见实现备注 9：不能取 `$SYSTEMA_RUNSTATEDIR/notify`——那是 syss 给服务用的 sd_notify socket 路径） |
| `SYSTEMA_RELOAD_SOCKET` | `$SYSTEMA_RUNSTATEDIR/sysf.sock` |
| `SYSTEMA_LOG_FILE`（见备注 12） | `${XDG_STATE_HOME:-$HOME/.local/state}/systema/log/systema-sysu.log`（启动器自身；各 worker 按同目录 `<binary>.log` 派生） |
| `SYSTEMA_SERVICE_LOG_DIR`（新，见 5.1） | `${XDG_STATE_HOME:-$HOME/.local/state}/systema/log/services` |
| `SYSTEMA_UNIT_PATH` | `$HOME/.config/systema:/etc/systemd/user:/usr/lib/systemd/user`（**整体替换**：绝不能继承默认值，否则会扫进 `/etc/systemd/system` 等系统目录） |
| `SYSTEMA_GENERATOR_PATH` | `$SYSTEMA_RUNSTATEDIR/systemd/generator:$SYSTEMA_RUNSTATEDIR/systemd/generator.late`（用户私有、当前为空） |

矩阵保持**纯 `SYSTEMA_*`**。总线地址不是矩阵条目：用户实例仅有的两个
D-Bus 消费者（sysw、syss 的 `Type=dbus` 轮询）由启动器在 spawn 时以
`--bus-address` 显式传入会话总线地址（备注 11），服务进程因此不继承任何
总线重定向——与 `systemd --user` 的进程环境一致。

保持编译期默认不动的：`SYSTEMA_BIN_PATH`、`SYSTEMA_LOCALE_DIR`、
`SYSTEMA_SHELL_PATH`、`SYSTEMA_SOCKET_HANDLER_PATH`、`SYSTEMD_MACHINE_ID_FILE`、
`SYSTEMD_LIB_UNIT_DIR`、`SYSTEMD_FIRST_BOOT_FILE`、`SYSTEMA_FINDER_PATH`
（这些指向系统只读资源，用户实例可直接读/执行）。

`XDG_RUNTIME_DIR` 天然 0700，用户实例的全部 socket 免费获得"仅本人 + root 可达"。

### 3.2 启动与就绪协议（复刻 sysi 的配方）

照 `systema-sysi/src/supervise.rs` 的三阶段，精简为用户域：

1. 计算环境矩阵，创建 `$SYSTEMA_RUNSTATEDIR`、`notify/`、日志目录。
2. **先**在 `$SYSTEMA_NOTIFY_DIR/init.sock` 绑定 notify 监听（datagram），
   再 spawn `systema-sysa`，等 `MANAGER_READY=1`。
3. 串行 spawn worker，各自等到 `WORKER_READY=<id>` 再起下一个：
   - `systema-syss`（`system-s-1`）
   - `systema-syst`（`system-t-1`）
   - `systema-sysw.systemd`（`system-w-1`，见备注 11：就绪事件由 sysa 在
     桥的控制 `manager.hello` 到达时广播，与 D-Bus 可用性无关）
   - **不包含**：sysr/sysm/sysd（系统域职责），sysc/sysk/sysn/syse（P2+）。
4. 控制阶段（连 `$SYSTEMA_CONTROL_SOCKET`，一调用一连接，同 sysi）：
   `manager.daemon_reload` → `manager.list_units(enabled_only)` →
   `manager.start_units`（确保含 `default.target`）。
   **跳过 `manager.register_power_units`**：power（reboot/halt）是系统域动作，
   用户实例不得注册。
5. 监督至退出：任一长驻子进程意外退出 → 整组优雅关停（TERM → 宽限 → KILL），
   与 sysi 行为一致。收到 SIGTERM/SIGINT 同样整组关停。

sysi 的 notify 监听实现（`NotifyListener`）在 sysi crate 内、非 libsysa；
按"不抽库"决策，sysu 内复制该监听的精简实现（约几十行）。

鉴权侧无需改动：`system_uid()` = `getuid()`（sysa 跑在用户 uid 上），
同一用户 CLI 直连即通过（`client_uid == sys_uid`），root 亦通过，他人拒绝。

### 3.3 单实例与幂等

sysa 自带单实例防护（ipc socket 可连即退出，`server.rs:45-54`）。
sysu 在 spawn 前先探测 `$SYSTEMA_CONTROL_SOCKET`：可连 → 打印"用户实例已在运行"
并以 0 退出（幂等，登录钩子可放心重复调用）。

### 3.4 登录钩子（P1）

- 提供 `systema-sysu --login` / `--logout` 两个便捷子命令（或直接用信号）：
  `--login` = 幂等启动（后台监督，父进程返回）；`--logout` = 向运行中的实例发 SIGTERM。
- 钩子接入方式：shell profile 一行启动 + `trap ... EXIT` 尽力停止。
  **已知局限**：SSH 断连/kill 会话时 EXIT trap 不可靠 → 残留实例可能存活到下次登录
  （下次 `--login` 幂等复用，不叠加）。确定性的登出停止属 P2（PAM 模块）。

## 4. 交付物 B：CLI `--user`

`systema-unitstatectl`、`systema-stagingctl` 各加一个**全局** `--user` 标志：

- 生效点：在 `paths::init()` **之前**扫描到 `--user` 后，把
  `SYSTEMA_CONTROL_SOCKET` 指向用户 socket（与 3.1 同一计算逻辑）。
  为避免两处 CLI 复制粘贴，在 `libsysa::paths` 增一个纯函数
  `user_control_socket() -> String`（只算路径，不改既有逻辑）。
- 帮助文本按既有模式：`mut_arg` + `t_()` 本地化；标志 `global = true`
  （与刚落地的 `--version` 同一形态）。
- `systema-stagingctl` 修正 `--uid` 默认值：`--user` 模式下 `commit` 不带
  `--uid` 时默认**调用者自己的 uid**，而非 0（`main.rs:402` 的 `unwrap_or(0)`）。
- 权限不足（连系统 socket 被拒）时输出明确提示：系统域操作需要 root，
  请加 `sudo`（呼应决策表：跨域控制 = 方案 A）。

`systema-sysw`（D-Bus 桥）与 syss 各加一个可选参数 `--bus-address`
（不传时行为一字不变，仍走 zbus 的系统总线默认）：用户实例 spawn 时把
会话总线地址传给两者，系统域 sysi 不传参（备注 11，随交付物 A 落地）。

## 5. 交付物 C：三个小修复（保持最小改动）

### 5.1 `/var/log/services` 硬编码

`systema-syss/src/process.rs:61` 的 `SERVICE_LOG_DIR` 是编译期常量，
用户实例无权写。改为读新环境变量 `SYSTEMA_SERVICE_LOG_DIR`，
默认值仍为 `/var/log/services`（系统域行为一字不变）。

### 5.2 `%m` machine-id：**无需修复**（结论修正）

原计划修 `systema-sysa/src/unit/parser.rs:464` 的编译期常量用法，
但核实后该文件是**死代码**：`unit/mod.rs` 只 `pub use systema_sysf_systemd::parser`，
并未声明 `mod parser`，该文件从不参与编译。
现行解析器（`systema-sysf-systemd/src/parser.rs:648`）已经使用运行时解析的
`paths::instance().systemd_machine_id_file`。
处置：删除死文件（纯清理，零行为变化）；如希望绝对零 diff，也可不动。

### 5.3 `User==self` 跳过凭证切换

用户域的 syss 以普通用户运行；`initgroups(2)` 需要 `CAP_SETGID`，
即便 `User=` 解析结果就是自己也会 EPERM，导致单元启动失败。
在凭证应用路径（`process.rs` 的 pre_exec 段）加最小判断：
目标 uid == 当前 euid 且 gid 未降权时跳过 `initgroups/setgid/setuid`（视为成功）。
系统域（root 跑 syss）行为不变。

## 6. 交付物 D：登出停止

P1 = 3.4 的 shell 钩子（尽力而为）；P2 = PAM 模块（确定性会话跟踪，
顺带创建 `session-*.scope`——当前树内没有任何东西创建该 scope，
`state.rs` 的 `user_sessions` 消费端已就绪，缺的就是这个创建端）。

## 7. 已知 P1 降级（可接受）

- 单元类型仅 service + target；timer/socket/path/scope/mount/slice 不在用户域。
- `Standard*=tty` 的用户单元：无 TTY 可挂，行为退化。
- 登出停止非确定（见 3.4）。
- 不注册 `.power` 单元；用户实例内 reboot/halt 无入口。

## 8. 验证清单

1. **宿主双实例冒烟**（系统实例 + 用户实例并存）：
   - `sudo systema-sysi`（系统域）与普通用户 `systema-sysu`（用户域）同时运行；
   - `systema-unitstatectl list` 见系统单元；`--user list` 见用户单元；两 socket 集合互不可见；
   - 写一个测试用户单元到 `~/.config/systema/`，经 sysu 控制阶段被加载、启动、
     停止；进程 uid = 本人；日志落在 `XDG_STATE_HOME` 下；
   - `systema-stagingctl --user list/commit`（默认 uid = 本人）；
   - 私有会话总线（`dbus-daemon --session` 于 `$XDG_RUNTIME_DIR/bus`）上，
     `systemctl --user list-units / status / start / stop` 经 sysw 闭环控制
     `test.service`（systemd ≥257 需 `SYSTEMCTL_FORCE_BUS=1`，见备注 11；
     证明 `org.freedesktop.systemd1` 由本实例的 sysw 承接）；
   - `--logout` 后用户实例整组退出（含 sysw），系统实例无扰；
   - `ls -l $XDG_RUNTIME_DIR/systema`：仅本人可入。
2. **五目标矩阵 0 error / 0 warning**：x86_64-unknown-linux-gnu、FreeBSD release
   （两个 `--config`）、OpenBSD、aarch64-linux-android、x86_64-linux-android release
   （`+nightly-2026-10-04`）。构建前清理 `target/`；注意 shell 残留 Termux 变量：
   `UNSET=$(env | grep -oE '^(SYSTEMA_|SYSTEMD_)[A-Za-z0-9_]+' | sed 's/^/-u /' | tr '\n' ' ')`，
   **zsh 下必须 `env ${=UNSET}`**（普通 `$UNSET` 不分词）。按
   `~/Projects/openbsd-cross/README.cargo.md` / `~/Projects/freebsd-cross/README.cargo.md`
   执行；Linux 上不 `cargo run` / `cargo test`。
3. **OpenBSD `rust-test.sh` 门禁**通过（"=== All Rust tests passed! ==="）。
4. 约定检查：`rustfmt --check` 仅针对改动文件；新增用户可见字符串后
   `make -C po update-po`；README 二进制表更新（17 → 18）。

## 9. 后续阶段（非 P1 承诺）

- **P2**：PAM 模块（确定性会话生命周期 + `session-*.scope` 创建）、
  用户域 timer/socket/path/scope worker。
  （原列此项的"session-bus `sysw` 用户域桥（真实 `systemctl --user` 兼容）"
  已提前进 P1，见备注 11。）
- **P3**：跨域授权引擎（`control.rs` 二元判定 → per-RPC 策略：peer cred +
  组/ACL 配置；复用既有 `user_sessions` 与 peer-cred 基建）、可选 Linux polkit 后端。
- **安全审计项**：sysw 桥接 `manager.*` 时只带 `sender_pid`
  （`systema-sysw.systemd/src/dbus/manager.rs:644,678`），
  未见对 D-Bus 调用方 uid 的校验；若系统总线 world-connectable，
  可能构成绕过 control socket uid 检查的旁路。无论 P 几都应补上
  调用方授权并让两条入口共用同一判定。
  （用户域实例跑在用户私有的 session bus 上，该旁路在 P1 不构成
  跨用户风险——备注 11；系统域入口的补课不受此影响。）

## 10. 修订（2026-10-25）：syss 的 PAM 会话改为驻留子进程执行（"(syss-pam)" / sd-pam 模式）

**背景**（deepin VM（deepin 6.6.x x86_64）现场诊断闭环，P1 落地后实测）：

- syss 原实现把整段 PAM 栈放在 tokio 阻塞池工作**线程**里跑（`spawn_blocking`）。
- 内核对 `/proc/<tgid>/loginuid` 的写有**任务身份闸**（非权限闸）：`fs/proc/base.c`
  的 `proc_loginuid_write()` 里 `current != pid_task(proc_pid(inode), PIDTYPE_PID)`
  → `-EPERM`——只有线程组 leader（"进程形态"任务）可写，与 CapEff/ptrace 全满与否
  无关（mainline v6.6 与 v7.3 源码逐字一致；VM 实测三分支：主线程 OK /
  fork 子进程 OK / 普通线程 EPERM——syss 工作线程 tid=3837 被 tracer 实抓）。
- `pam_loginuid.so` 在 `systemd-user` 栈中是 required → 栈整体返回 PAM 14
  （`PAM_SESSION_ERR`）→ PAM 环境永远为空：`XDG_RUNTIME_DIR` 只能靠
  `xdg_runtime_dir_fallback` 兜底，`DBUS_SESSION_BUS_ADDRESS` 要等
  `dbus.socket` 的 ExecStartPost 在 ~20 s 后补上 → dde-session 的一次性引导
  （exec 前的环境即需完成）早于该时间点而失败，桌面链不启动。

**决策**：照 systemd `setup_pam()` 的 "*(sd-pam)*" 形态重做。每次携带 `PAMName=`
的 spawn，由 syss `fork()` 一个专用驻留子进程：

1. 子进程跑完整栈（`pam_start` → `set_item(PAM_TTY)` → `setcred(ESTABLISH)` →
   `pam_open_session` → `pam_getenvlist`），把环境列表经管道按
   「状态字节 + NUL 分隔条目」交回父进程，然后**驻留**持有 pamh 直至服务主进程
   退出；最后**在同一执行上下文**完成收尾序列
   （`pam_close_session` → `setcred(DELETE)` → `pam_end`）。
2. 父进程带超时（30 s）读环境管道；失败语义与原实现一致：warn 后无 PAM 环境
   继续启动，`xdg_runtime_dir_fallback`（§5 中 `XDG_RUNTIME_DIR` 的兜底）不变。
3. fork 卫生：libpam 在 fork 前 dlopen 并进程级缓存；CString 全部预转换；
   子进程关闭除 stdio + 两条管道外的全部 fd；不碰 tokio/tracing；一律
   `_exit()` 结束；Linux/Android 上设 `PR_SET_PDEATHSIG=SIGKILL`
   （syss 死则陪葬；fork 与 prctl 之间有 getppid 复查）与
   `PR_SET_NAME="(syss-pam)"`；两条管道 O_CLOEXEC——此后父进程 fork/exec 的
   服务子进程绝不继承 lifecycle 写端，EOF 信号才真正随服务退出产生。
4. 收尾触发：沿用既有 monitor 任务（服务主进程退出判定）→ 关 lifecycle 写端
   → 子进程收尾 → 父进程有界 waitpid（≤10 s，超时 SIGKILL）；`ECHILD`
   （被 reaper 预收）与 `EINTR` 均视为完成。异常兜底：spawn 失败路径立即收尸；
   `PamSession` 意外 Drop 只关写端（EOF 让子进程自毁），遗留收尸由
   `reaper.rs`（子进程不在 registry 快照内）完成。
5. 子进程生命周期内只保留 stdio（PAM 模块经 `/dev/log` 的 syslog 通路照常可用）。

**不改变**：PAM 可选性（libpam 缺失即跳过）、触发条件（`PAMName=` + `User=` +
非 NO_SETUID）、调用点位置（`process.rs` spawn 段）、monitor 循环骨架。

**部署注意**：fork 子进程继承 syss 的 loginuid 设置态，而
`AUDIT_FEATURE_LOGINUID_IMMUTABLE` 默认开 → **syss 必须随 boot 链启动**
（loginuid=unset）。从已登录 shell 手工拉起时连接 fork 的子进程都会在
audit 闸（`kernel/audit.c:audit_set_loginuid_perm()`）EPERM。deepin 6.6
没有 audit 相关 sysctl 不影响本规则（纯 procfs 身份判定）。

**验证**：syss 日志不再出现 PAM 14；`/proc/<user-mgr 的 pid>/environ` 在
spawn 即刻同含 `XDG_RUNTIME_DIR` 与 `DBUS_SESSION_BUS_ADDRESS`（不再由
ExecStartPost 延迟 20 s 补）；修复前 VM 现象（dde-session-manager 因缺
DBUS 变量引导失败）消失。

## 附：关键代码位置

| 内容 | 位置 |
|------|------|
| 路径运行时解析 / env 覆盖 | `crates/libsysa/src/paths.rs`（`compute_paths`、`builtin_defaults`、`debug_dump`） |
| sysi 三阶段配方 | `crates/backend/systema-sysi/src/supervise.rs`（`control_phase` ~L705；notify 监听 ~L49；worker 集 `workers.rs:67`） |
| sysa 单实例防护 / `system_uid()` | `crates/backend/systema-sysa/src/ipc/server.rs:45-54, 260` |
| control 面鉴权 | `crates/backend/systema-sysa/src/ipc/control.rs:312-313` |
| `manager.daemon_reload` / `start_units` 处理 | `crates/backend/systema-sysa/src/ipc/control.rs:528, 1193` |
| syss 日志目录（`SYSTEMA_SERVICE_LOG_DIR` 运行时覆盖） | `crates/backend/systema-syss/src/process.rs`（`service_log_dir()`） |
| syss 凭证切换（initgroups，无操作跳过） | `crates/backend/systema-syss/src/process.rs`（pre_exec 段） |
| syss 驻留 PAM 子进程（"(syss-pam)"，§10） | `crates/backend/systema-syss/src/pam.rs`（`pam_session_open` / `pam_child_main`）；触发点 `process.rs`（spawn 段、monitor 收尾） |
| 死文件 | 已删除：`crates/backend/systema-sysa/src/unit/parser.rs`（`unit/mod.rs` 从未引用它） |
| CLI 接入点 | `crates/frontend/systema-unitstatectl/src/main.rs`；`crates/frontend/systema-stagingctl/src/main.rs` |
| admin 客户端（读 `control_socket_path`） | `crates/libsysa/src/staging_admin.rs`、`unitstate_admin.rs`（经 `ipc::connect_control`） |
| 用户域环境矩阵 / 运行时目录 | `crates/backend/systema-sysu/src/envs.rs`；`crates/libsysa/src/paths.rs`（`user_runtime_dir`、`user_control_socket`） |
| sysu 监督配方 | `crates/backend/systema-sysu/src/supervise.rs`（`control_phase` 无 power 注册） |
| 用户域 sysw / session-bus 定向 | `crates/backend/systema-sysu/src/workers.rs`（worker 集）、`envs.rs`（`session_bus_address()`）、`supervise.rs`（spawn 追加 `--bus-address`）；`--bus-address` 选项：`systema-sysw.systemd/src/{main.rs,bridge.rs,dbus/mod.rs}`、`systema-syss/src/{main.rs,ipc/mod.rs,dbus.rs}` |

## 实现备注（P1 落地）

以下为实施时对正文的少量细化，不改变决策语义：

1. **权限提示的输出位置**（§4 "CLI 在权限不足时输出提示"）：落在两处，
   由 CLI 原样显示——
   - 连接被拒（EACCES）：`libsysa/src/ipc.rs` 的 `connect_control()`，仅在
     `PermissionDenied` 分支的 "Failed to connect to System A: {e}" 后追加提示；
   - 已连通但 uid 不符（服务端拒绝）：`sysa/src/ipc/control.rs` 的
     `privilege_hint()` 追加到两条 `Permission denied …` 拒绝消息尾部。
   采用服务端追加而非客户端按字符串匹配：拒绝消息本身走 `l10n` 翻译，
   客户端匹配会随语言失效；`stagingctl`/`unitstatectl` 是仅有的
   `admin.*` 消费方，效果等同于 CLI 输出提示。
2. **`paths::user_runtime_dir()`**：除正文的 `user_control_socket()` 外还新增
   纯函数 `user_runtime_dir()`——启动器矩阵（§3.1）与 `--user` 必须计算出
   同一运行时目录，同源于一处。
3. **`--login` 机制**：父进程以隐藏标志 `--daemon` 重执行自身；子进程
   `setsid()` 脱离会话，stdio 继承自父进程——不再重定向（子进程由
   `logging::init_daemon` 自行双写 stderr + 日志文件，见备注 12）。
   pidfile = `{user_runtime_dir}/systema-sysu.pid`，在监督开始前写入；
   父进程轮询 pidfile（≤3s），以区分"已起来"与"启动即失败"。
4. **控制阶段启动失败不致命**：`manager.start_units` 的 enqueue 级失败
   （典型：会话尚未创建 `default.target`）记 warn 后继续，实例保持运行
   ——与 sysi 的 `bail` 不同；RPC 级失败（连接/握手）仍然中止。
   此外启动列表为空（全新会话、无任何用户单元）时直接跳过 start 调用。
5. **运行时目录 0700**：启动器创建 `$SYSTEMA_RUNSTATEDIR` 时显式 chmod 0700
   ——`/tmp/systema-<uid>` 回退路径不能靠父目录隔离，验收清单依赖此权限。
6. **§5.3 的判定**实现为 `euid != 0 && creds.uid == euid && creds.gid == egid`：
   root（系统域）恒走原路径；`Group=`-only 且 gid 为自身的失败情形保持不变。
7. **stagingctl `commit --uid` 默认值**：去掉 `default_value_t = 0`，运行时解析
   ——`--user` 且未给 `--uid` 时取 `geteuid()`，否则 0。
8. **UNIT_PATH 无 `HOME` 时**：跳过 `$HOME/.config/systema` 条目，余下
   `/etc/systemd/user:/usr/lib/systemd/user`（登录会话必有 HOME，
   该分支仅为健壮性）。
9. **`SYSTEMA_NOTIFY_DIR` 偏离 §3.1 原值**（原值 `$SYSTEMA_RUNSTATEDIR/notify`）：
   syss 给服务用的 sd_notify socket 路径正是 `{runstatedir}/notify`
   （`syss/src/notify.rs` 的 `notify_socket_path()`），同一路径不能既是
   sysu/sysi 存放 `init.sock` 的**目录**又是 syss 的 socket **文件**
   （实测 syss 每次启动 EADDRINUSE 告警、Type=notify 就绪等待被跳过）。
   系统域默认无此冲突，靠的正是 notify 目录多一层 `/systema/` 段
   （`{runstatedir}/systema/notify`，`libsysa/build.rs`）。故用户域同构取
   `$SYSTEMA_RUNSTATEDIR/systema/notify`，把 `{runstatedir}/notify`
   留给 syss；`libsysa/notify.rs` 的广播按目录取监听 socket，不受影响。
10. **`--user` 的实例提前探测**：判据不是"socket 文件是否存在"而是
   **真实 connect 探测**——文件残留但无人监听（登出/崩溃后的
   ECONNREFUSED）与文件不存在（ENOENT）同样输出"请先
   `systema-sysu --login`"的友好提示；其余连接错误（如 EACCES）
   放行给 admin 客户端，由备注 1 的权限提示接管。实例在线时该探测
   会在服务端留下一行 `disconnected before the first envelope` 的
   info 日志（`control.rs` 对 EOF 返回 Ok，非错误）。
11. **sysw 提前进 P1**（修订 §2 兼容性行、§3.2 步骤 3、§4、§9）：用户实例的
   worker 集增加 `systema-sysw.systemd`（`system-w-1`），让会话里的真实
   `systemctl --user`（D-Bus `org.freedesktop.systemd1`）直接控制本实例。
   总线定向经**命令行选项**而非环境变量（初稿的矩阵变量
   `DBUS_SYSTEM_BUS_ADDRESS` 语义不明——"指向会话总线的系统总线变量"，
   2026-10-10 定稿同日修订）：
   - **`--bus-address <ADDR>`**：sysw 与 syss 各加一个可选参数，启动时
     解析为 `zbus::Address`（非法值 fail-fast 拒绝启动）；连接一律改经
     `Builder::address()`（sysw 的占名探针与服务端、syss 的 `Type=dbus`
     `BusName=` 轮询）。不传参时走 zbus 默认 `Address::system()`——
     系统域（sysi 不传参）行为一字不变，且仍合法尊重环境里真实的
     `DBUS_SYSTEM_BUS_ADDRESS`；worker 自身不感知 system/user 之分，
     域知识留在启动器。
   - **启动器传参**：sysu spawn sysw 与 syss 时追加
     `--bus-address <session_bus_address()>`（`envs.rs`），地址推导的
     优先级与 `sd_bus_open_user()` 一致：已设的 `DBUS_SESSION_BUS_ADDRESS`
     原样 → `$XDG_RUNTIME_DIR/bus` → `{user_runtime_dir}/bus`（死路兜底：
     该会话无总线时只会空转重连——约每秒一轮控制会话重建的日志噪声，
     进程存活、实例不受影响——而绝不会落回
     `/run/dbus/system_bus_socket`，域隔离因此是构造性的）。
     矩阵（§3.1）因此保持纯 `SYSTEMA_*`：**服务进程不继承任何总线
     重定向**，与 `systemd --user` 的进程环境完全一致（初稿"实例全域
     注入"造成的 `Connection::system()` 偏离随之取消）。
   - syss 的 `Type=dbus` `BusName=` 轮询经同一选项指向 session bus——
     与 `systemd --user` 对用户单元的语义一致（systemd 上游确认用户
     管理器看会话总线）。
   - **就绪门不依赖 D-Bus**：sysw 的 `WORKER_READY=system-w-1` 由 sysa
     在控制 `manager.hello`（worker_id 非空）时广播（`ipc/control.rs`），
     桥连不上总线也不会拖住启动。
   - **systemctl 的传输层**（冒烟实测，systemd v262）：systemd ≥ v257
     （PR #34686）起 `systemctl --user` 默认直连管理器私有 socket
     `$XDG_RUNTIME_DIR/systemd/private`，该 socket 缺失时**不再回退**
     会话总线，直接报 "Failed to connect to user scope bus via
     local transport"。P1 用户实例不提供该私有直连面——它不是加个监听
     就行：名字占用与信号路由由 bus daemon 提供，zbus 需以 `p2p`
     server 模式逐连接服务（`Builder::server`，`p2p` feature），
     属后续阶段（**P2 候选**）。现即可用的两条路：
     ① 会话导出 `SYSTEMCTL_FORCE_BUS=1`（systemctl 内建开关，强制走
     会话总线）→ 落到 sysw，`list-units / status / start / stop /
     is-active` 实测全部闭环（冒烟 phase E 即如此验证）；
     ② 无 `XDG_RUNTIME_DIR` 而有 `DBUS_SESSION_BUS_ADDRESS` 的老式会话
     中 systemd 保留 ENOMEDIUM 回退，裸 `systemctl --user` 直接可用。
     `busctl --user` 及任何 D-Bus 客户端**今天就直接可用**（sysw 面
     完整：ListUnits、unit 对象、JobRemoved 信号）。
   - **名字冲突**：会话总线上已有 `org.freedesktop.systemd1`
     （真实 `systemd --user` 在跑）时沿用 sysw 既有行为：告警 + 1s
     重连循环（进程存活，实例无扰，`systemctl` 落到既有管理器上）。
   - **授权**：session bus 用户私有，`manager.*` 桥只带 `sender_pid`
     的旁路（§9 审计项）在用户域不构成跨用户风险；系统域入口仍须补。
12. **全局日志方案改版（修订 §3.1 矩阵行、备注 3）**：所有组件统一走
   `libsysa::logging` 的同一套初始化，永远双写 **stderr + 日志文件**
   两个 sink，不再镜像 `/dev/console`（`SYSTEMA_CONSOLE`、
   `console-is-tty` 判断一并移除）。日志文件路径解析优先级
   **`--log-file`（运行参数）> `SYSTEMA_LOG_FILE`（环境变量）> 内置默认**
   `/var/log/systema/<name>.log`；值 `-` 表示仅 stderr。文件打不开
   （早启动 `/var` 未就绪、无权限等）时降级为仅 stderr 并告警一次，
   进程绝不因日志路径失败而退出。启动器/监督者 spawn 子进程时不再
   重定向 stdio、也不再注入环境变量，而是逐个传
   `--log-file <自身日志所在目录>/<binary>.log`（argv 优先于继承的
   环境变量）；仅 stderr 的父进程给子进程传 `--log-file -`。
   交互式 CLI（`stagingctl`/`unitstatectl`）策略不同：不显式给
   `--log-file` 就不写文件，不给 `--debug` 就绝不往 stderr 输出日志。
   §3.1 矩阵中的 `SYSTEMA_LOG_DIR` 因此改为 `SYSTEMA_LOG_FILE`
   （值为启动器自身日志文件；各 worker 的目录由 argv 派生）；
   `SYSTEMA_SERVICE_LOG_DIR` 语义不变（服务日志是另一套机制）。
