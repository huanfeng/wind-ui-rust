# Final Fix Report

## MEDIUM-1：bind 持锁串行化

**文件**：`src/sync.rs`，`WakerShared::bind`

**改动**：将原来「swap pending → signal → 安装 raw」的三步改为全程持锁、先装 raw 再检查 pending：

```rust
pub(crate) fn bind(self: &Arc<Self>, raw: RawWake) {
    // 全程持锁：与同样持锁的 wake() 串行化 raw 的读写，消除「pending 已读、raw 未装」的窗口。
    let mut guard = self.raw.lock().unwrap();
    *guard = Some(raw);
    if self.pending.swap(false, Ordering::SeqCst) {
        guard.as_ref().unwrap().signal();
    }
}
```

`wake()` 已持同一把锁读写 raw，两者完全串行，竞态窗口消除。

## MEDIUM-2：批处理契约注释

**文件**：`src/app.rs`，`UiHost::render` 开头 pump 循环上方

**改动**：注释扩充为三行，说明「一帧排空所有积压 + 单一 Waker 不可拆分」契约。

## 测试结果

### sync:: 专项测试

```
cargo test --lib sync::

running 4 tests
test sync::tests::send_after_receiver_dropped_errs ... ok
test sync::tests::waker_is_send_sync ... ok
test sync::tests::wake_before_bind_is_pending_then_flushed ... ok
test sync::tests::channel_pump_drains_in_order_across_thread ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 134 filtered out
```

`wake_before_bind_is_pending_then_flushed` 确认行为不变：先 wake 置 pending、bind 补发一次、再 wake 直接 signal。

### 全量测试

```
cargo test --lib

test result: ok. 138 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

### clippy

```
cargo clippy --lib

Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.19s
```

无新增告警。
