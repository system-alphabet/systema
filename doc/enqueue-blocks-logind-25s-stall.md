# 冷启动登录 25 秒停顿：`manager.enqueue` 等待前置 job 导致 logind 死锁(根因与修复计划)

状态：**已定位,证据链闭合,修复进行中**（2026-10-31,VM 192.168.100.238 实测,`--debug` 日志 + system bus 抓包）。
与 [login-pam-15s-stall.md](./login-pam-15s-stall.md) 是同一类“串行链 × logind 同步调用”问题的另一处出口;
15 秒那处已修,本文是剩下的 25 秒。

## 1. 现象

- 冷启动后首次由 DDM 登录:`user@1000` 拉起前,`systemd-user-runtime-dir start 1000`
  固定停顿 **~25.0 s**(13:52:17.749 → 13:52:42.765),期间无任何相关日志,最终 exit 0。
- 该次登录 DDE 报 `failed to set systemd1 envs` → 无任务栏;再次登录(此时
  `user-runtime-dir@1000` 已 active)则正常。systemd 引导下两种情况都正常。
- 25 s 等于 sd-bus 默认方法调用超时 `BUS_DEFAULT_TIMEOUT`
  (`systemd/src/libsystemd/sd-bus/bus-internal.h`)。

## 2. 证据(system bus 抓包,epoch 秒的小数部分)

| 时刻 | 事件 |
|---|---|
| 737.728 | logind 在 `CreateSession` 内同步调用 `systemd1.StartUnit("user@1000.service")` |
| 737.749 | sysa 计划 `[user-runtime-dir@1000 start, user@1000 start]`,打出 `Waiting to dispatch user@1000 for predecessor jobs` |
| 737.753 | `systemd-user-runtime-dir` 向 logind 同步 `Properties.Get(RuntimeDirectorySize)`,logind 无应答 |
| 737.7 → 762.7 | 无人推进:logind 等 `StartUnit` 回复;sysw 等 sysa 的 `manager.enqueue` 回复;sysa 等 user-runtime-dir 结束;user-runtime-dir 等 logind |
| 762.744 | 两条 25 s 超时同时到期:user-runtime-dir 的属性读取静默回落默认值并 exit 0;logind 的 `StartUnit` 同步调用超时 |
| 762.772 | sysa 的前置完成,`manager.enqueue` 才返回,sysw 此时才回 `StartUnit`(logind 已放弃等待) |

logind 采样(`/proc/<pid>/wchan`)空闲于 `ep_poll`,排除了 logind 自身卡死;
`user-runtime-dir.c` 对 login1 属性读取失败是静默回落的,所以没有任何错误日志。

## 3. 根因

`crates/backend/systema-sysa/src/scheduler/mod.rs` 的 `enqueue_job` 在串行模式
(`Replace` / `ReplaceIrreversibly`)下,于调度循环里**内联** `wait_for_predecessors(..).await`,
最后才 `Ok(primary_job_id)`。因此 `manager.enqueue` 的回复被前置 job 的完成所阻塞。

systemd 的契约是 `StartUnit` 入队即回(`bus_unit_queue_job()` 在 job 运行前就回复 job 路径),
调用者通过 `JobRemoved` 得知结果。logind 的 `CreateSession` 正是靠这点:
`manager_start_unit` 是同步调用,`user@N` 的 job 又 `BindsTo=/After=user-runtime-dir@N`,
而 user-runtime-dir 需要 logind 应答属性读取——只要 `StartUnit` 不立即返回就闭环成环。

sysw 一侧(`systema-sysw.systemd/src/dbus/manager.rs::enqueue_job`)的设计注释已按入队即回写成;
违约的是 sysa 的 `manager.enqueue` 处理链。

控制会话是逐条处理请求的(`ipc/control.rs` 的 `reader.next()` → `dispatch(..).await`),
所以这次阻塞也会挡住同一连接上排在后面的 `manager.*` 请求。

“第二次才成功”:user-runtime-dir 是 `RemainAfterExit=yes`,第二次已 active,
`-EALREADY` 短路让 `enqueue` 无需等待前置,环不成立。

## 4. 修复计划

范围限定在 System A 调度器,不改 proto、不改方法名(见 [ipc-api-stability.md](./ipc-api-stability.md)),不涉及平台差异。

1. **派发尾段后台化**:`enqueue_job` 循环里,每个 step 在建好 job 记录、发出 `JobNew`、
   起好超时监视、构建完 `method.call` 信封之后,把
   “等前置 → 发信封 → 发送失败的善后”整体放进一个 `tokio::spawn` 任务,
   循环继续处理下一个 step,函数随即返回 `primary_job_id`。
   - 任务只持有克隆(`allocator`、`step_marks` 的 receiver、信封字节、`worker_envelope_tx`);
   - 发送失败的善后(置 job 为 Failed、对 root 发 `JobCompletion::Failed`)原样搬入任务;
   - 仍是 `current_thread` 运行时,`spawn` 不引入线程;
   - 串行语义不变:每个 step 仍等待其 `wait_for` 前置的 mark 后才发出。
2. **不改动的部分**:`resource.prepare`(`Delegate=`)仍在入队路径内联,它是本地 round-trip,
   有 30 s 上界且毫秒级返回(本次日志 1 ms 内),不在这条环上;若后续证据显示它也会阻塞再单独处理。
3. **回归测试**(`scheduler` 的 `mod tests`,fake worker):
   - 两个 step 的串行计划,前置 job 的 fake worker 不回复;断言 `enqueue_job` 在前置完成**之前**就返回;
   - 随后让前置完成,断言后继的 `method.call` 才被发出(顺序语义保持)。
4. **验证**:`cargo test -p systema-sysa`;当前平台 0 error / 0 warning 构建;
   `target/` 在 btrfs,按 AGENTS.md 无需先清理,不做交叉编译(除非另行要求)。
   改动文件执行 `rustfmt --check`。
5. **实机验证(由用户执行)**:重新部署后冷启动登录,期望 `user-runtime-dir@1000` 的
   `Starting → exited` 间隔回到毫秒级,DDE 首次登录即有任务栏。

## 5. 已排除

- logind 重启/卡死(采样空闲于 `ep_poll`);
- polkit 未就绪(登录窗口内已 active);
- sysw 广泛订阅/监听导致的延迟;
- 15 秒调度器 “dispatching anyway”(已于前次修复,本次日志不再出现)。

## 6. 修复后的附带问题:`Transaction::delete_job` 递归栈溢出

部署上述修复后冷启动,sysa 在 `manager.start_units` → `build_plan_multi` →
`collect_garbage` → `delete_job(.., true)` 中栈溢出(core 栈深 ~8800 帧,
idx 3 ↔ 54 两个 job 互为 outgoing 反复出现)。

- 原因:`delete_job` 先对 `outgoing` 递归,再摘除自身的边;依赖图里存在环
  (a 拉入 b、b 又拉入 a)时永不终止。该代码在本次修复前就存在,
  与 `manager.enqueue` 的派发后台化无关(栈完全位于同步的计划构建内)。
- 修复:与 systemd `transaction_unlink_job()` 一致,先摘除自身的边与 arena 槽位,
  再对记下的 `outgoing` 递归。回归测试
  `delete_job_with_dependencies_terminates_on_a_cycle`。

## 7. 修复后的附带问题:开机批量 `start_units` 判定 `Transaction order is cyclic`

现象:`graphical.target (Start, Replace) failed: Transaction order is cyclic`,
`manager.start_units: cannot start 'default.target'`。

- 机理(由代码推断,**尚未用实机 debug 日志证实**):sysi 的开机控制阶段把
  所有 enabled 单元依次交给 `manager.start_units`,每个名字单独规划一次。
  规划器把所有 `Running` 的 job 当作 `installed`,它们在序环检测里不可删除。
  原先 `enqueue_job` 要等到各步骤派发(即前置完成)才返回,下一个根的规划
  基本在前一批落定后;后台化后批量调用立刻返回,`default.target` 在数百个
  在途 job 之上规划,环上有不可删的 installed 节点就只能报 Cyclic。
- 修复:`manager.start_units` 改用 `enqueue_start_after_dispatch`,保留“等到所有步骤
  已派发”的背压(派发任务仍是 spawn 的,15 秒超时只取消等待,不再丢弃未派发步骤);
  总线路径(`manager.enqueue`,logind 的 `StartUnit`)仍入队即回。
- 若部署后仍报 Cyclic,说明推断不成立,需要 `--debug` 日志(含 `Deleting … to break
  ordering cycle`)继续取证。
