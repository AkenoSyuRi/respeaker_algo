//! 算法 Pipeline worker：有界队列接收 CaptureBlock，固定 256-frame hop 后驱动 PipelineRuntime。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};

use crate::audio::{CaptureBlock, RESPEAKER_CHANNELS, RESPEAKER_MIC_CHANNELS};
use crate::beamformer::BeamformerStats;
use crate::doa::HOP_SIZE;
use crate::pipeline::{PipelineConfig, PipelineInputBlock, PipelineRuntime};

const PIPELINE_QUEUE_CAPACITY: usize = 32;

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PipelineWorkerStats {
    pub capture_blocks: u64,
    pub input_frames: u64,
    pub processed_hops: u64,
    pub max_queue_depth: usize,
    pub bf_stats: Option<BeamformerStats>,
}

pub struct PipelineWorkerHandle {
    tx: Option<SyncSender<CaptureBlock>>,
    join: Option<JoinHandle<Result<PipelineWorkerStats, String>>>,
    queue_depth: Arc<AtomicUsize>,
    max_queue_depth: Arc<AtomicUsize>,
    first_error: Arc<OnceLock<String>>,
}

impl PipelineWorkerHandle {
    #[allow(dead_code)]
    pub fn spawn(config: PipelineConfig, out_dir: String, prefix: String) -> Result<Self, String> {
        Self::spawn_with_publisher(config, out_dir, prefix, None)
    }

    pub fn spawn_with_publisher(
        config: PipelineConfig,
        out_dir: String,
        prefix: String,
        publisher: Option<crate::events::EventPublisher>,
    ) -> Result<Self, String> {
        let (tx, rx) = sync_channel::<CaptureBlock>(PIPELINE_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let max_queue_depth = Arc::new(AtomicUsize::new(0));
        let first_error = Arc::new(OnceLock::new());
        let depth_worker = Arc::clone(&queue_depth);
        let max_worker = Arc::clone(&max_queue_depth);
        let error_worker = Arc::clone(&first_error);

        let join = thread::Builder::new()
            .name("algorithm-worker".into())
            .spawn(move || {
                let runtime =
                    match PipelineRuntime::new_with_publisher(config, &out_dir, &prefix, publisher)
                    {
                        Ok(r) => {
                            let _ = ready_tx.send(Ok(()));
                            r
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e.clone()));
                            return Err(e);
                        }
                    };
                run_worker(rx, runtime, depth_worker, max_worker, error_worker)
            })
            .map_err(|e| format!("创建 algorithm worker 失败: {e}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: Some(tx),
                join: Some(join),
                queue_depth,
                max_queue_depth,
                first_error,
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                Err("algorithm worker 启动握手失败".into())
            }
        }
    }

    pub fn try_push(&self, block: CaptureBlock) -> Result<(), String> {
        if let Some(error) = self.first_error.get() {
            return Err(error.clone());
        }
        let Some(tx) = self.tx.as_ref() else {
            return Err("Pipeline input closed".into());
        };
        let reserved = self.queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
        match tx.try_send(block) {
            Ok(()) => {
                let mut max = self.max_queue_depth.load(Ordering::Relaxed);
                while reserved > max {
                    match self.max_queue_depth.compare_exchange_weak(
                        max,
                        reserved,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(current) => max = current,
                    }
                }
                Ok(())
            }
            Err(TrySendError::Full(_)) => {
                self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                match self.first_error.get() {
                    Some(error) => Err(error.clone()),
                    None => Err("Pipeline queue overrun".into()),
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                self.queue_depth.fetch_sub(1, Ordering::Relaxed);
                match self.first_error.get() {
                    Some(error) => Err(error.clone()),
                    None => Err("pipeline worker disconnected".into()),
                }
            }
        }
    }

    /// 只关闭输入，不等待 worker。
    pub fn close_input(&mut self) {
        self.tx.take();
    }

    /// 若输入尚未关闭则先关闭，然后 join worker。
    pub fn finish(&mut self) -> Result<PipelineWorkerStats, String> {
        self.close_input();
        match self.join.take() {
            Some(join) => match join.join() {
                Ok(result) => result,
                Err(_) => Err("algorithm worker panic".into()),
            },
            None => Ok(PipelineWorkerStats {
                max_queue_depth: self.max_queue_depth.load(Ordering::Relaxed),
                ..PipelineWorkerStats::default()
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn stalled_for_test(capacity: usize) -> (Self, Receiver<CaptureBlock>) {
        let (tx, rx) = sync_channel(capacity);
        (
            Self {
                tx: Some(tx),
                join: None,
                queue_depth: Arc::new(AtomicUsize::new(0)),
                max_queue_depth: Arc::new(AtomicUsize::new(0)),
                first_error: Arc::new(OnceLock::new()),
            },
            rx,
        )
    }
}

impl Drop for PipelineWorkerHandle {
    fn drop(&mut self) {
        self.close_input();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct PipelineHopAssembler {
    start_frame: u64,
    filled: usize,
    algo: [i16; HOP_SIZE],
    mic: [i16; HOP_SIZE * RESPEAKER_MIC_CHANNELS],
    reference: [i16; HOP_SIZE],
}

impl PipelineHopAssembler {
    fn new() -> Self {
        Self {
            start_frame: 0,
            filled: 0,
            algo: [0; HOP_SIZE],
            mic: [0; HOP_SIZE * RESPEAKER_MIC_CHANNELS],
            reference: [0; HOP_SIZE],
        }
    }

    fn push_block(
        &mut self,
        block: &CaptureBlock,
        runtime: &mut PipelineRuntime,
        stats: &mut PipelineWorkerStats,
    ) -> Result<(), String> {
        let samples = &block.samples;
        let mut local_frame = 0usize;
        while local_frame < block.frames {
            if self.filled == 0 {
                self.start_frame = block.start_frame + local_frame as u64;
            }
            let src = local_frame * RESPEAKER_CHANNELS;
            self.algo[self.filled] = samples[src];
            let mic_off = self.filled * RESPEAKER_MIC_CHANNELS;
            self.mic[mic_off..mic_off + 4].copy_from_slice(&samples[src + 1..src + 5]);
            self.reference[self.filled] = samples[src + 5];
            self.filled += 1;
            local_frame += 1;
            if self.filled == HOP_SIZE {
                self.emit(runtime, stats, HOP_SIZE)?;
            }
        }
        Ok(())
    }

    fn finish(
        &mut self,
        runtime: &mut PipelineRuntime,
        stats: &mut PipelineWorkerStats,
    ) -> Result<(), String> {
        if self.filled > 0 {
            self.emit(runtime, stats, self.filled)?;
        }
        Ok(())
    }

    fn emit(
        &mut self,
        runtime: &mut PipelineRuntime,
        stats: &mut PipelineWorkerStats,
        frames: usize,
    ) -> Result<(), String> {
        runtime.push_block(PipelineInputBlock {
            start_frame: self.start_frame,
            frames,
            algo: &self.algo[..frames],
            mic: &self.mic[..frames * RESPEAKER_MIC_CHANNELS],
            reference: &self.reference[..frames],
        })?;
        stats.processed_hops += 1;
        self.filled = 0;
        Ok(())
    }
}

fn run_worker(
    rx: Receiver<CaptureBlock>,
    mut runtime: PipelineRuntime,
    queue_depth: Arc<AtomicUsize>,
    max_queue_depth: Arc<AtomicUsize>,
    worker_error: Arc<OnceLock<String>>,
) -> Result<PipelineWorkerStats, String> {
    let mut stats = PipelineWorkerStats::default();
    let mut assembler = PipelineHopAssembler::new();
    let mut expected_start_frame = 0u64;
    let mut first_err: Option<String> = None;

    let mut expected_sequence = 0u64;
    while let Ok(block) = rx.recv() {
        let prev = queue_depth.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0);
        stats.capture_blocks += 1;
        stats.input_frames += block.frames as u64;

        if let Err(e) = block.validate() {
            let _ = worker_error.set(e.clone());
            first_err = Some(e);
            break;
        }
        if block.sequence != expected_sequence || block.start_frame != expected_start_frame {
            let error = format!(
                "algorithm worker 收到不连续块: got seq={} start={}, expected seq={} start={}",
                block.sequence, block.start_frame, expected_sequence, expected_start_frame
            );
            let _ = worker_error.set(error.clone());
            first_err = Some(error);
            break;
        }
        expected_sequence += 1;
        expected_start_frame += block.frames as u64;

        if let Err(e) = assembler.push_block(&block, &mut runtime, &mut stats) {
            let _ = worker_error.set(e.clone());
            first_err = Some(e);
            break;
        }
    }

    // 先销毁 receiver，使发送端在耗时 finalize 前立即观察到 Disconnected。
    drop(rx);

    if first_err.is_none()
        && let Err(e) = assembler.finish(&mut runtime, &mut stats)
    {
        let _ = worker_error.set(e.clone());
        first_err = Some(e);
    }
    let finalize_result = runtime.finalize();
    stats.bf_stats = runtime.beamformer_stats();
    if let Err(e) = finalize_result {
        let _ = worker_error.set(e.clone());
        first_err.get_or_insert(e);
    }

    stats.max_queue_depth = max_queue_depth.load(Ordering::Relaxed);
    match first_err {
        Some(e) => Err(e),
        None => Ok(stats),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PipelineConfig;
    use std::sync::mpsc::sync_channel;

    fn silent_block(sequence: u64, start_frame: u64, frames: usize) -> CaptureBlock {
        CaptureBlock::from_samples(
            sequence,
            start_frame,
            vec![0i16; frames * RESPEAKER_CHANNELS],
        )
        .unwrap()
    }

    #[test]
    fn hop_assembler_emits_exact_256_frame_blocks() {
        let config = PipelineConfig::parse("version = 1").unwrap();
        let mut runtime = PipelineRuntime::new(config, "target/out", "hop_exact").unwrap();
        let mut assembler = PipelineHopAssembler::new();
        let mut stats = PipelineWorkerStats::default();
        let block = silent_block(0, 0, 512);
        assembler
            .push_block(&block, &mut runtime, &mut stats)
            .unwrap();
        assert_eq!(stats.processed_hops, 2);
        assert_eq!(assembler.filled, 0);
        runtime.finalize().unwrap();
    }

    #[test]
    fn hop_assembler_is_capture_packet_size_independent() {
        let config = PipelineConfig::parse("version = 1").unwrap();
        let mut runtime = PipelineRuntime::new(config, "target/out", "hop_indep").unwrap();
        let mut assembler = PipelineHopAssembler::new();
        let mut stats = PipelineWorkerStats::default();
        let mut start = 0u64;
        for (seq, frames) in [100usize, 30, 200, 182].into_iter().enumerate() {
            let block = silent_block(seq as u64, start, frames);
            assembler
                .push_block(&block, &mut runtime, &mut stats)
                .unwrap();
            start += frames as u64;
        }
        assert_eq!(start, 512);
        assert_eq!(stats.processed_hops, 2);
        runtime.finalize().unwrap();
    }

    #[test]
    fn hop_assembler_emits_partial_tail_on_finish() {
        let config = PipelineConfig::parse("version = 1").unwrap();
        let mut runtime = PipelineRuntime::new(config, "target/out", "hop_tail").unwrap();
        let mut assembler = PipelineHopAssembler::new();
        let mut stats = PipelineWorkerStats::default();
        assembler
            .push_block(&silent_block(0, 0, 100), &mut runtime, &mut stats)
            .unwrap();
        assert_eq!(stats.processed_hops, 0);
        assembler.finish(&mut runtime, &mut stats).unwrap();
        assert_eq!(stats.processed_hops, 1);
        runtime.finalize().unwrap();
    }

    #[test]
    fn worker_rejects_sequence_gap() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enable_viewer = false
"#,
        )
        .unwrap();
        let mut worker =
            PipelineWorkerHandle::spawn(config, "target/out".into(), "gap_seq".into()).unwrap();
        worker.try_push(silent_block(0, 0, 16)).unwrap();
        worker.try_push(silent_block(2, 16, 16)).unwrap();
        worker.close_input();
        let err = worker.finish().unwrap_err();
        assert!(err.contains("不连续"), "{err}");
    }

    #[test]
    fn worker_rejects_start_frame_gap() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enable_viewer = false
"#,
        )
        .unwrap();
        let mut worker =
            PipelineWorkerHandle::spawn(config, "target/out".into(), "gap_start".into()).unwrap();
        worker.try_push(silent_block(0, 0, 16)).unwrap();
        worker.try_push(silent_block(1, 32, 16)).unwrap();
        worker.close_input();
        let err = worker.finish().unwrap_err();
        assert!(err.contains("不连续"), "{err}");
    }

    #[test]
    fn worker_processing_error_is_visible_before_finish() {
        let config = PipelineConfig::parse("version = 1").unwrap();
        let mut worker =
            PipelineWorkerHandle::spawn(config, "target/out".into(), "visible_error".into())
                .unwrap();
        worker.try_push(silent_block(0, 0, 16)).unwrap();
        worker.try_push(silent_block(2, 16, 16)).unwrap();

        let mut observed = None;
        for _ in 0..100_000 {
            if let Some(error) = worker.first_error.get() {
                observed = Some(error.clone());
                break;
            }
            thread::yield_now();
        }
        let expected = observed.expect("worker 应在 finish 前发布处理错误");
        let push_error = worker
            .try_push(silent_block(3, 32, 16))
            .expect_err("worker 首错后不得继续接收块");
        assert_eq!(push_error, expected);
        assert!(push_error.contains("不连续"), "{push_error}");
        let finish_error = worker.finish().unwrap_err();
        assert_eq!(finish_error, expected);
    }

    #[test]
    fn worker_returns_beamformer_stats() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "beamformer"
direction_source = "fixed"
wav = false
"#,
        )
        .unwrap();
        let mut worker =
            PipelineWorkerHandle::spawn(config, "target/out".into(), "bf_stats_manifest".into())
                .unwrap();
        worker.try_push(silent_block(0, 0, HOP_SIZE)).unwrap();
        worker.close_input();
        let stats = worker.finish().unwrap();
        let bf_stats = stats.bf_stats.unwrap();
        assert_eq!(bf_stats.input_frames, HOP_SIZE as u64);
    }

    #[test]
    fn pipeline_try_push_reports_full() {
        let (tx, _rx) = sync_channel::<CaptureBlock>(1);
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let max_queue_depth = Arc::new(AtomicUsize::new(0));
        let handle = PipelineWorkerHandle {
            tx: Some(tx),
            join: None,
            queue_depth: Arc::clone(&queue_depth),
            max_queue_depth,
            first_error: Arc::new(OnceLock::new()),
        };
        handle.try_push(silent_block(0, 0, 1)).unwrap();
        let err = handle.try_push(silent_block(1, 1, 1)).unwrap_err();
        assert!(err.contains("overrun"), "{err}");
        assert_eq!(queue_depth.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn queue_depth_preincrement_rolls_back_on_full() {
        let (tx, _rx) = sync_channel::<CaptureBlock>(1);
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let max_queue_depth = Arc::new(AtomicUsize::new(0));
        let handle = PipelineWorkerHandle {
            tx: Some(tx),
            join: None,
            queue_depth: Arc::clone(&queue_depth),
            max_queue_depth: Arc::clone(&max_queue_depth),
            first_error: Arc::new(OnceLock::new()),
        };
        handle.try_push(silent_block(0, 0, 1)).unwrap();
        assert!(handle.try_push(silent_block(1, 1, 1)).is_err());
        assert_eq!(queue_depth.load(Ordering::Relaxed), 1);
        assert_eq!(max_queue_depth.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn queue_depth_preincrement_rolls_back_on_disconnect() {
        let (tx, rx) = sync_channel::<CaptureBlock>(1);
        drop(rx);
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let max_queue_depth = Arc::new(AtomicUsize::new(0));
        let handle = PipelineWorkerHandle {
            tx: Some(tx),
            join: None,
            queue_depth: Arc::clone(&queue_depth),
            max_queue_depth,
            first_error: Arc::new(OnceLock::new()),
        };
        assert!(handle.try_push(silent_block(0, 0, 1)).is_err());
        assert_eq!(queue_depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn queue_depth_never_underflows_when_receiver_runs_immediately() {
        let (tx, rx) = sync_channel::<CaptureBlock>(1);
        let queue_depth = Arc::new(AtomicUsize::new(0));
        let max_queue_depth = Arc::new(AtomicUsize::new(0));
        let depth = Arc::clone(&queue_depth);
        let maxd = Arc::clone(&max_queue_depth);
        let worker = thread::spawn(move || {
            let block = rx.recv().unwrap();
            let prev = depth.fetch_sub(1, Ordering::Relaxed);
            assert!(prev > 0);
            assert_eq!(block.sequence, 0);
            let _ = maxd;
        });
        let handle = PipelineWorkerHandle {
            tx: Some(tx),
            join: None,
            queue_depth: Arc::clone(&queue_depth),
            max_queue_depth: Arc::clone(&max_queue_depth),
            first_error: Arc::new(OnceLock::new()),
        };
        handle.try_push(silent_block(0, 0, 1)).unwrap();
        worker.join().unwrap();
        assert_eq!(queue_depth.load(Ordering::Relaxed), 0);
        assert!(max_queue_depth.load(Ordering::Relaxed) <= 2);
    }

    #[test]
    fn close_input_does_not_block_waiting_for_worker() {
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "doa"
enable_viewer = false
"#,
        )
        .unwrap();
        let mut worker =
            PipelineWorkerHandle::spawn(config, "target/out".into(), "close_input".into()).unwrap();
        worker.try_push(silent_block(0, 0, 256)).unwrap();
        worker.close_input();
        // 不应阻塞；finish 才 join。
        let stats = worker.finish().unwrap();
        assert!(stats.processed_hops >= 1);
    }

    #[test]
    fn worker_init_error_is_returned_before_spawn_success() {
        // 用固定的非法输出路径构造稳定的 worker 初始化失败。
        let config = PipelineConfig::parse(
            r#"
version = 1
[[modules]]
type = "beamformer"
direction_source = "fixed"
wav = true
"#,
        )
        .unwrap();
        let err =
            PipelineWorkerHandle::spawn(config, "target/out/\0bad".into(), "init_fail".into());
        assert!(err.is_err(), "预期初始化失败");
    }
}
