//! 主循环心跳与 in-flight 观测（2026-10-31 卡死事故的观测闭环）。
//!
//! 病灶：主循环 6 处内联 `executor.execute().await`，投递链串联 HTTP 无总帽
//! ——任一环节挂起即全桥停摆（diary/热更/消息全停、日志零痕迹），而看门狗
//! TCP 探活只看端口（serve 是独立任务）必然误报健康。本模块给主循环装
//! 「心跳」、给长阻塞段打「在忙什么」标——`/api/health` 与 diagnostics
//! 暴露，看门狗据此区分「忙」（in_flight 在且未超时）与「死」（无 in_flight
//! 而心跳停）。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn now_ms() -> u64 {
    T0.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 单个进行中的长阻塞段（投递/下载等）；单槽——主循环串行执行，槽里是
/// 最后开始的一段（此前超时放弃的后台滞留投递会被覆盖，见 executor 总帽）。
#[derive(Debug, Clone)]
pub struct InFlight {
    pub kind: String,
    pub key: String,
    pub started_at: Instant,
}

pub struct WatchState {
    last_beat_ms: AtomicU64,
    in_flight: Mutex<Option<InFlight>>,
}

impl Default for WatchState {
    fn default() -> Self {
        Self {
            last_beat_ms: AtomicU64::new(now_ms()),
            in_flight: Mutex::new(None),
        }
    }
}

impl WatchState {
    pub fn shared() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// 主循环每 tick 打一次（500ms 一拍；正常 age < 1s）。
    pub fn beat(&self) {
        self.last_beat_ms.store(now_ms(), Ordering::SeqCst);
    }

    /// 长阻塞段开始/结束打点；嵌套时内层覆盖（begin 记录、end 只在 kind
    /// 匹配时清除，防外层段被内层的 end 误清）。
    pub fn begin(&self, kind: &str, key: &str) {
        *self.in_flight.lock().unwrap() = Some(InFlight {
            kind: kind.to_string(),
            key: key.to_string(),
            started_at: Instant::now(),
        });
    }

    pub fn end(&self, kind: &str) {
        let mut g = self.in_flight.lock().unwrap();
        if g.as_ref().is_some_and(|f| f.kind == kind) {
            *g = None;
        }
    }

    /// 心跳是否在 `secs` 内（判活阈值取 10s：正常 500ms 一拍，偶发单次
    /// 阻塞（sync_dirty 落盘等）远小于此）。
    pub fn alive_within(&self, secs: u64) -> bool {
        now_ms().saturating_sub(self.last_beat_ms.load(Ordering::SeqCst)) < secs * 1000
    }

    /// health/diagnostics 暴露形状：`{mainLoopAlive, heartbeatAgeMs, inFlight}`。
    pub fn snapshot(&self) -> serde_json::Value {
        let age = now_ms().saturating_sub(self.last_beat_ms.load(Ordering::SeqCst));
        let inflight = self
            .in_flight
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| {
                serde_json::json!({
                    "kind": f.kind,
                    "key": f.key,
                    "elapsedSec": f.started_at.elapsed().as_secs(),
                })
            })
            .unwrap_or(serde_json::Value::Null);
        serde_json::json!({
            "mainLoopAlive": self.alive_within(10),
            "heartbeatAgeMs": age,
            "inFlight": inflight,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 心跳判活窗口与打点生命周期。
    #[test]
    fn beat_and_in_flight_lifecycle() {
        let w = WatchState::default();
        assert!(w.alive_within(10), "新建即有心跳");
        // in_flight 空快照。
        assert!(w.snapshot()["inFlight"].is_null());
        // begin/end 配对清除；end 带 kind 校验防误清外层段。
        w.begin("deliver", "group:1");
        let snap = w.snapshot();
        assert_eq!(snap["inFlight"]["kind"], "deliver");
        assert_eq!(snap["inFlight"]["key"], "group:1");
        w.begin("download", "v0.4.18");
        w.end("deliver"); // kind 不匹配：不清（download 仍在）。
        assert_eq!(w.snapshot()["inFlight"]["kind"], "download");
        w.end("download");
        assert!(w.snapshot()["inFlight"].is_null());
    }
}
