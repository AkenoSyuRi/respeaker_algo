//! 置信度门控状态机 + 圆周恒角速度 Kalman。
//!
//! 状态：`Searching → Tracking ⇄ Coasting`。
//! - 获取：连续 3 帧高置信度且方向一致（≤20°），用候选圆周均值初始化 Kalman；
//! - 更新：`confidence >= update_confidence` 且 `|innovation| <= 60°`；
//! - 跳变：连续 3 帧高置信度候选（与预测相差 >60°）后重置；
//! - 丢失：超过 `max_coast_ms` 无有效更新则回到 `Searching`。

use crate::doa::srp::Observation;
use crate::doa::{HOP_SIZE, SAMPLE_RATE, circular_distance_deg, wrap_360};

const ACQUIRE_CONSISTENCY_DEG: f32 = 20.0;
const INNOVATION_GATE_DEG: f32 = 60.0;
const ACQUIRE_FRAMES: u32 = 3;
const JUMP_FRAMES: u32 = 3;
const SIGMA_ACCEL_DEG_S2: f32 = 360.0;
const OMEGA_CLAMP_DEG_S: f32 = 360.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackStatus {
    Searching,
    Tracking,
    Coasting,
}

/// 2×2 Kalman（状态 [theta_unwrapped_rad, omega_rad_s]）。
struct Kalman2x2 {
    x: [f32; 2],
    p: [[f32; 2]; 2],
    initialized: bool,
}

fn wrap_pi(x: f32) -> f32 {
    (x + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

impl Kalman2x2 {
    fn new() -> Self {
        Self {
            x: [0.0; 2],
            p: [[0.0; 2]; 2],
            initialized: false,
        }
    }

    fn init_at(&mut self, theta_rad: f32) {
        let var_angle = (10.0_f32).to_radians().powi(2);
        let var_vel = (180.0_f32).to_radians().powi(2);
        self.x = [theta_rad, 0.0];
        self.p = [[var_angle, 0.0], [0.0, var_vel]];
        self.initialized = true;
    }

    fn reset(&mut self) {
        self.initialized = false;
    }

    fn predict(&mut self, dt: f32) {
        if !self.initialized {
            return;
        }
        let theta = self.x[0] + dt * self.x[1];
        let omega = self.x[1];
        self.x = [theta, omega];

        // P' = F P F^T + Q
        let p00 = self.p[0][0];
        let p01 = self.p[0][1];
        let p11 = self.p[1][1];
        // F = [[1, dt], [0, 1]]
        let n00 = p00 + 2.0 * dt * p01 + dt * dt * p11;
        let n01 = p01 + dt * p11;
        let n11 = p11;
        let sigma_a = SIGMA_ACCEL_DEG_S2.to_radians();
        let dt2 = dt * dt;
        let q00 = sigma_a * sigma_a * dt2 * dt2 / 4.0;
        let q01 = sigma_a * sigma_a * dt2 * dt / 2.0;
        let q11 = sigma_a * sigma_a * dt2;
        self.p = [[n00 + q00, n01 + q01], [n01 + q01, n11 + q11]];

        self.x[1] = self.x[1].clamp(
            -OMEGA_CLAMP_DEG_S.to_radians(),
            OMEGA_CLAMP_DEG_S.to_radians(),
        );
        self.enforce_finite();
    }

    /// 用圆周残差更新：innovation = wrap_pi(measured_wrapped - predicted_wrapped)。
    fn update(&mut self, measured_wrapped_rad: f32, confidence: f32) {
        if !self.initialized {
            return;
        }
        let predicted_wrapped = wrap_pi(self.x[0]);
        let innovation = wrap_pi(measured_wrapped_rad - predicted_wrapped);

        let sigma_meas_deg = 3.0 + 22.0 * (1.0 - confidence).powi(2);
        let r = sigma_meas_deg.to_radians().powi(2);
        // H = [1, 0]；K = P H^T / (H P H^T + R)
        let hpht = self.p[0][0] + r;
        if !hpht.is_finite() || hpht <= 0.0 {
            return;
        }
        let k0 = self.p[0][0] / hpht;
        let k1 = self.p[0][1] / hpht;

        // x += K * innovation
        self.x[0] += k0 * innovation;
        self.x[1] += k1 * innovation;

        // P = (I - K H) P
        let p00 = self.p[0][0];
        let p01 = self.p[0][1];
        let p11 = self.p[1][1];
        self.p[0][0] = (1.0 - k0) * p00;
        self.p[0][1] = (1.0 - k0) * p01;
        self.p[1][0] = -k1 * p00 + p01;
        self.p[1][1] = -k1 * p01 + p11;

        // 保持对称
        let avg = (self.p[0][1] + self.p[1][0]) * 0.5;
        self.p[0][1] = avg;
        self.p[1][0] = avg;

        self.x[1] = self.x[1].clamp(
            -OMEGA_CLAMP_DEG_S.to_radians(),
            OMEGA_CLAMP_DEG_S.to_radians(),
        );
        self.enforce_finite();
    }

    fn enforce_finite(&mut self) {
        for v in &mut self.x {
            if !v.is_finite() {
                *v = 0.0;
            }
        }
        for row in &mut self.p {
            for v in row {
                if !v.is_finite() {
                    *v = 0.0;
                }
            }
        }
    }

    /// 当前角度（弧度，未 wrap 的连续值）。
    fn theta_rad(&self) -> f32 {
        self.x[0]
    }
}

/// 3 帧候选累积（用于获取与跳变）。
struct CandidateRun {
    count: u32,
    last_deg: f32,
    sum_cos: f32,
    sum_sin: f32,
}

impl CandidateRun {
    fn new() -> Self {
        Self {
            count: 0,
            last_deg: 0.0,
            sum_cos: 0.0,
            sum_sin: 0.0,
        }
    }

    /// 加入一个候选；若与上一候选循环距离 > 一致性阈值则重置计数。
    fn push(&mut self, deg: f32, max_gap_deg: f32) {
        if self.count == 0 || circular_distance_deg(deg, self.last_deg) <= max_gap_deg {
            self.count += 1;
            self.last_deg = deg;
            let r = deg.to_radians();
            self.sum_cos += r.cos();
            self.sum_sin += r.sin();
        } else {
            self.count = 1;
            self.last_deg = deg;
            let r = deg.to_radians();
            self.sum_cos = r.cos();
            self.sum_sin = r.sin();
        }
    }

    fn reset(&mut self) {
        self.count = 0;
        self.sum_cos = 0.0;
        self.sum_sin = 0.0;
    }

    fn mean_deg(&self) -> f32 {
        self.sum_sin
            .atan2(self.sum_cos)
            .to_degrees()
            .rem_euclid(360.0)
    }

    fn ready(&self, need: u32) -> bool {
        self.count >= need
    }
}

/// 置信度门控状态机。
pub struct GateTracker {
    kalman: Kalman2x2,
    status: TrackStatus,
    coast_ms: f64,
    acquire: CandidateRun,
    jump: CandidateRun,
    acquire_confidence: f32,
    update_confidence: f32,
    max_coast_ms: u32,
}

impl GateTracker {
    pub fn new(acquire_confidence: f32, update_confidence: f32, max_coast_ms: u32) -> Self {
        Self {
            kalman: Kalman2x2::new(),
            status: TrackStatus::Searching,
            coast_ms: 0.0,
            acquire: CandidateRun::new(),
            jump: CandidateRun::new(),
            acquire_confidence,
            update_confidence,
            max_coast_ms,
        }
    }

    pub fn status(&self) -> TrackStatus {
        self.status
    }

    /// 当前跟踪角度（度，内部数学坐标）。测试辅助接口。
    #[allow(dead_code)]
    pub fn tracked_deg(&self) -> Option<f32> {
        if self.kalman.initialized {
            Some(wrap_360(self.kalman.theta_rad().to_degrees()))
        } else {
            None
        }
    }

    /// 每帧驱动一次。`obs` 为当前帧观测（无观测时按丢失处理）。
    /// 返回 (观测是否用于 Kalman 更新, 当前预测/更新后的内部角度(度, 未转换))。
    pub fn update(&mut self, obs: Option<Observation>) -> (bool, Option<f32>) {
        let dt = HOP_SIZE as f32 / SAMPLE_RATE as f32;
        self.kalman.predict(dt);
        let predicted_internal_deg = self
            .kalman
            .initialized
            .then(|| self.kalman.theta_rad().to_degrees());

        let Some(o) = obs else {
            self.coast(dt);
            return (false, self.tracked_deg());
        };

        let measured_internal = o.raw_internal_deg;
        let conf = o.confidence;

        match self.status {
            TrackStatus::Searching => {
                if conf >= self.acquire_confidence {
                    self.acquire
                        .push(measured_internal, ACQUIRE_CONSISTENCY_DEG);
                    if self.acquire.ready(ACQUIRE_FRAMES) {
                        let mean = self.acquire.mean_deg();
                        self.kalman.init_at(mean.to_radians());
                        self.status = TrackStatus::Tracking;
                        self.acquire.reset();
                        self.coast_ms = 0.0;
                        return (true, Some(mean));
                    }
                } else {
                    self.acquire.reset();
                }
                (false, None)
            }
            TrackStatus::Tracking | TrackStatus::Coasting => {
                // 正常更新门控
                let innovation_gate_ok = match predicted_internal_deg {
                    Some(p) => {
                        circular_distance_deg(measured_internal, wrap_360(p)) <= INNOVATION_GATE_DEG
                    }
                    None => true,
                };
                if conf >= self.update_confidence && innovation_gate_ok {
                    self.kalman.update(measured_internal.to_radians(), conf);
                    self.status = TrackStatus::Tracking;
                    self.coast_ms = 0.0;
                    self.jump.reset();
                    let deg = wrap_360(self.kalman.theta_rad().to_degrees());
                    return (true, Some(deg));
                }
                // 大角度跳变候选
                if conf >= self.acquire_confidence {
                    self.jump.push(measured_internal, ACQUIRE_CONSISTENCY_DEG);
                    if self.jump.ready(JUMP_FRAMES) {
                        let mean = self.jump.mean_deg();
                        self.kalman.init_at(mean.to_radians());
                        self.status = TrackStatus::Tracking;
                        self.jump.reset();
                        self.coast_ms = 0.0;
                        return (true, Some(mean));
                    }
                } else {
                    self.jump.reset();
                }
                // 无有效更新：coast
                self.coast(dt);
                (false, self.tracked_deg())
            }
        }
    }

    fn coast(&mut self, dt: f32) {
        // 未获取目标（Searching）时，无观测保持 Searching，不进入 Coasting。
        if self.status == TrackStatus::Searching {
            return;
        }
        self.coast_ms += dt as f64 * 1000.0;
        if self.coast_ms >= self.max_coast_ms as f64 {
            self.status = TrackStatus::Searching;
            self.kalman.reset();
            self.acquire.reset();
            self.jump.reset();
            self.coast_ms = 0.0;
            return;
        }
        self.status = TrackStatus::Coasting;
        // 无观测时对角速度施加轻微阻尼
        if self.kalman.initialized {
            self.kalman.x[1] *= 0.98;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doa::srp::Observation;

    fn obs(deg: f32, conf: f32) -> Observation {
        Observation {
            raw_internal_deg: deg,
            confidence: conf,
            peak_score: 1.0,
            second_peak_score: 0.0,
            peak_gap_ratio: 0.5,
            prominence: 5.0,
            mean_msc: 0.6,
            rms_dbfs: -20.0,
        }
    }

    #[test]
    fn crossing_zero_unwraps() {
        let mut t = GateTracker::new(0.65, 0.40, 500);
        let seq = [350.0f32, 355.0, 359.0, 1.0, 5.0, 10.0];
        let mut prev: Option<f32> = None;
        for &deg in &seq {
            let (_, _) = t.update(Some(obs(deg, 0.95)));
            let cur = t.tracked_deg();
            if let (Some(p), Some(c)) = (prev, cur) {
                let d = circular_distance_deg(c, p);
                assert!(d <= 15.0, "跨 0° 出现大跳变: {p} -> {c}");
            }
            prev = cur;
        }
        let final_deg = t.tracked_deg().unwrap();
        assert!(
            circular_distance_deg(final_deg, 10.0) <= 8.0,
            "最终角度 {final_deg} 应接近 10°"
        );
        assert_eq!(t.status(), TrackStatus::Tracking);
    }

    #[test]
    fn acquire_gate() {
        let mut t = GateTracker::new(0.65, 0.40, 500);
        // 2 帧高置信度：不获取
        t.update(Some(obs(20.0, 0.9)));
        t.update(Some(obs(21.0, 0.9)));
        assert_eq!(t.status(), TrackStatus::Searching);
        assert!(t.tracked_deg().is_none());
        // 第 3 帧一致方向：获取
        t.update(Some(obs(22.0, 0.9)));
        assert_eq!(t.status(), TrackStatus::Tracking);
        let deg = t.tracked_deg().unwrap();
        assert!(circular_distance_deg(deg, 21.0) <= 10.0);

        // 中间插入低置信度：计数重置
        let mut t2 = GateTracker::new(0.65, 0.40, 500);
        t2.update(Some(obs(30.0, 0.9)));
        t2.update(Some(obs(31.0, 0.5))); // 低置信度
        t2.update(Some(obs(32.0, 0.9)));
        t2.update(Some(obs(33.0, 0.9)));
        assert_eq!(t2.status(), TrackStatus::Searching, "计数应被低置信度重置");

        // 方向不一致：不获取
        let mut t3 = GateTracker::new(0.65, 0.40, 500);
        t3.update(Some(obs(10.0, 0.9)));
        t3.update(Some(obs(60.0, 0.9))); // 与上一候选差 50° > 20°
        t3.update(Some(obs(12.0, 0.9)));
        assert_eq!(t3.status(), TrackStatus::Searching);
    }

    #[test]
    fn large_jump_resets_after_three() {
        let mut t = GateTracker::new(0.65, 0.40, 500);
        // 稳定在 20°
        for _ in 0..10 {
            t.update(Some(obs(20.0, 0.9)));
        }
        assert_eq!(t.status(), TrackStatus::Tracking);
        // 高置信度 200° 跳变观测
        t.update(Some(obs(200.0, 0.9)));
        t.update(Some(obs(201.0, 0.9)));
        let mid = t.tracked_deg().unwrap();
        assert!(
            circular_distance_deg(mid, 20.0) <= 30.0,
            "前两次跳变观测不应立即重置（当前 {mid}）"
        );
        t.update(Some(obs(202.0, 0.9)));
        let after = t.tracked_deg().unwrap();
        assert!(
            circular_distance_deg(after, 200.0) <= 15.0,
            "第 3 次一致跳变后应重置到约 200°（当前 {after}）"
        );
    }

    #[test]
    fn coast_then_search() {
        let mut t = GateTracker::new(0.65, 0.40, 500);
        // 获取
        for _ in 0..3 {
            t.update(Some(obs(45.0, 0.9)));
        }
        assert_eq!(t.status(), TrackStatus::Tracking);
        // 连续低置信度
        let frames_500ms = 500 / 16; // ~31 帧
        for i in 0..frames_500ms + 5 {
            let (_, returned) = t.update(Some(obs(45.0, 0.05)));
            if i < frames_500ms {
                assert_eq!(
                    t.status(),
                    TrackStatus::Coasting,
                    "第 {i} 帧应处于 Coasting"
                );
                assert!(t.tracked_deg().is_some(), "coast 期间应保留预测角度");
            } else if t.status() == TrackStatus::Searching {
                assert!(returned.is_none(), "丢失目标后不得返回旧的跟踪角度");
            }
        }
        assert_eq!(
            t.status(),
            TrackStatus::Searching,
            "超过 500ms 应回到 Searching"
        );
        assert!(t.tracked_deg().is_none());
    }

    #[test]
    fn low_confidence_never_updates() {
        let mut t = GateTracker::new(0.65, 0.40, 500);
        // 获取后持续低置信度
        for _ in 0..3 {
            t.update(Some(obs(90.0, 0.9)));
        }
        assert_eq!(t.status(), TrackStatus::Tracking);
        // 低置信度观测角度完全不同也不应更新轨迹（innovation 门 60° 也会拒绝）
        for _ in 0..5 {
            let (used, _) = t.update(Some(obs(300.0, 0.1)));
            assert!(!used);
        }
    }
}
