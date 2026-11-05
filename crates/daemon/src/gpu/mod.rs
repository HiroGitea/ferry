//! GPU inventory and live metrics.

use std::sync::Arc;

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct RawGpu {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    pub mem_total: u64,
    pub mem_used: u64,
    pub util_percent: u32,
    pub temp_c: Option<u32>,
    pub power_w: Option<f32>,
}

pub trait GpuProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn list(&self) -> Vec<RawGpu>;
    /// GPUs currently assigned to jobs (only used by the fake provider to synthesise load).
    fn set_busy(&self, _busy: &[u32]) {}
}

pub struct NoGpu;
impl GpuProvider for NoGpu {
    fn name(&self) -> &'static str {
        "none"
    }
    fn list(&self) -> Vec<RawGpu> {
        vec![]
    }
}

/// Development provider: `n` fake 80 GB GPUs. GPUs assigned to jobs show synthetic load so the
/// dashboard has something to draw.
pub struct FakeGpu {
    pub n: u32,
    busy: std::sync::Mutex<Vec<u32>>,
}

impl FakeGpu {
    pub fn new(n: u32) -> Self {
        Self {
            n,
            busy: std::sync::Mutex::new(vec![]),
        }
    }
}

impl GpuProvider for FakeGpu {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn set_busy(&self, busy: &[u32]) {
        *self.busy.lock().unwrap() = busy.to_vec();
    }
    fn list(&self) -> Vec<RawGpu> {
        let busy = self.busy.lock().unwrap().clone();
        let t = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
        (0..self.n)
            .map(|i| {
                let on = busy.contains(&i);
                let wave = ((t / 20.0 + i as f64).sin() * 0.5 + 0.5) as f32;
                let jitter = (((t * 7.3 + i as f64 * 13.0).sin()) * 0.5 + 0.5) as f32;
                let util = if on {
                    70.0 + 25.0 * wave + 5.0 * jitter
                } else {
                    0.0
                };
                RawGpu {
                    index: i,
                    name: "Fake GPU 80GB".into(),
                    uuid: format!("GPU-fake-{i}"),
                    mem_total: 80 << 30,
                    mem_used: if on {
                        ((30.0 + 20.0 * wave) as u64) << 30
                    } else {
                        0
                    },
                    util_percent: util.min(100.0) as u32,
                    temp_c: Some(if on { 60 + (15.0 * wave) as u32 } else { 32 }),
                    power_w: Some(if on { 250.0 + 120.0 * wave } else { 45.0 }),
                }
            })
            .collect()
    }
}

#[cfg(feature = "nvml")]
pub mod nvml {
    use super::{GpuProvider, RawGpu};
    use nvml_wrapper::Nvml;

    pub struct NvmlGpu {
        nvml: Nvml,
    }

    impl NvmlGpu {
        pub fn try_new() -> Option<Self> {
            match Nvml::init() {
                Ok(nvml) => Some(Self { nvml }),
                Err(e) => {
                    tracing::info!("NVML unavailable ({e}); running without GPUs");
                    None
                }
            }
        }
    }

    impl GpuProvider for NvmlGpu {
        fn name(&self) -> &'static str {
            "nvml"
        }
        fn list(&self) -> Vec<RawGpu> {
            let count = self.nvml.device_count().unwrap_or(0);
            (0..count)
                .filter_map(|i| {
                    let d = self.nvml.device_by_index(i).ok()?;
                    let mem = d.memory_info().ok();
                    Some(RawGpu {
                        index: i,
                        name: d.name().unwrap_or_else(|_| "unknown".into()),
                        uuid: d.uuid().unwrap_or_default(),
                        mem_total: mem.as_ref().map(|m| m.total).unwrap_or(0),
                        mem_used: mem.as_ref().map(|m| m.used).unwrap_or(0),
                        util_percent: d.utilization_rates().map(|u| u.gpu).unwrap_or(0),
                        temp_c: d
                            .temperature(
                                nvml_wrapper::enum_wrappers::device::TemperatureSensor::Gpu,
                            )
                            .ok(),
                        power_w: d.power_usage().ok().map(|mw| mw as f32 / 1000.0),
                    })
                })
                .collect()
        }
    }
}

pub fn from_config(cfg: &Config) -> Arc<dyn GpuProvider> {
    let spec = cfg.gpu_provider.trim();
    if let Some(n) = spec.strip_prefix("fake:") {
        return Arc::new(FakeGpu::new(n.parse().unwrap_or(1)));
    }
    match spec {
        "none" => Arc::new(NoGpu),
        _ => {
            #[cfg(feature = "nvml")]
            {
                if let Some(p) = nvml::NvmlGpu::try_new() {
                    return Arc::new(p);
                }
            }
            Arc::new(NoGpu)
        }
    }
}
