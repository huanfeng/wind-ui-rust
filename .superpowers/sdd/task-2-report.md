# Task 2 报告：typed channel Sender + 类型擦除 pump

## 做了什么

仅修改 `src/sync.rs`，在现有 `WakerShared`/`Waker` 之前插入：

1. `pub use std::sync::mpsc::SendError;` — 重导出，供调用方无需 use std
2. `pub struct Sender<Msg>` — 持 `mpsc::Sender<Msg>` + `Waker`，手动 `impl Clone`
3. `impl<Msg> Sender<Msg>::send` — 先 `tx.send`，成功后调 `waker.wake()`，`?` 透传错误
4. `pub(crate) fn new_channel` — 建 `mpsc::channel`，pump 闭包持 `rx` + `on_message`，每调一次排空队列

## TDD 步骤与输出

### Step 1：追加测试 + stub（`unimplemented!()`）

```
# 编辑 src/sync.rs：插入 Sender struct + stub 实现 + 两个测试
```

### Step 2：确认测试失败

```
$ cargo test --lib sync::tests::channel_pump
running 1 test
test sync::tests::channel_pump_drains_in_order_across_thread ... FAILED
  panicked at src\sync.rs:37:5: not implemented
test result: FAILED. 0 passed; 1 failed
```

### Step 3：替换为真实实现

### Step 4：全部 sync 测试通过

```
$ cargo test --lib sync::
running 4 tests
test sync::tests::send_after_receiver_dropped_errs ... ok
test sync::tests::wake_before_bind_is_pending_then_flushed ... ok
test sync::tests::waker_is_send_sync ... ok
test sync::tests::channel_pump_drains_in_order_across_thread ... ok
test result: ok. 4 passed; 0 failed
```

### Step 5：clippy

```
$ cargo clippy --lib
warning: 9 warnings（全为 dead_code，后续任务接入后消除）
无新增告警
```

## 自审

- `Sender` 自动满足 `Send + Sync`（当 `Msg: Send`），无需手写 unsafe
- pump 持有 `rx` 所有权，`drop(pump)` 即关闭接收端，`send_after_receiver_dropped_errs` 测试验证了此语义
- 消息顺序由 `mpsc` FIFO 保证，`channel_pump_drains_in_order_across_thread` 验证跨线程顺序正确
- 无 unsafe、无额外依赖、无调试代码残留
