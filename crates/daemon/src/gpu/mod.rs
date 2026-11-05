//! GPU inventory and live metrics.

use std::sync::Arc;

use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, Serialize)]
pub struct RawGpu {
    pub index: u32,
    pub name: String,
    pub uuid: String,
    pub mem_total: u64,
    pub mem_used: u64,
    pub util_percent: u32,
    pub temp_c: Option<u32>,
}

pub trait GpuProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn list(&self) -> Vec<RawGpu>;
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

pub struct FakeGpu(pub u32);
impl GpuProvider for FakeGpu {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn list(&self) -> Vec<RawGpu> {
        (0..self.0)
            .map(|i| RawGpu {
                index: i,
                name: "Fake GPU 80GB".into(),
                uuid: format!("GPU-fake-{i}"),
                mem_total: 80 << 30,
                mem_used: 0,
                util_percent: 0,
                temp_c: Some(30),
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
                    })
                })
                .collect()
        }
    }
}

pub fn from_config(cfg: &Config) -> Arc<dyn GpuProvider> {
    let spec = cfg.gpu_provider.trim();
    if let Some(n) = spec.strip_prefix("fake:") {
        return Arc::new(FakeGpu(n.parse().unwrap_or(1)));
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
