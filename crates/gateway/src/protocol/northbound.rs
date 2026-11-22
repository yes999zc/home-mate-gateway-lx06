//! Optional diagnostics schema 1 mirrored from Core contracts-v0.md; wire remains 0/0.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct NorthboundStatus {
    pub schema: u32,
    pub boot_id: String,
    pub link_generation: u64,
    pub sampled_uptime_ms: u64,
    pub critical_current_count: u64,
    pub critical_high_water_count: u64,
    pub critical_current_bytes: u64,
    pub critical_high_water_bytes: u64,
    pub health_coalesced: u64,
    pub presence_coalesced: u64,
    pub critical_full: u64,
    pub invalid_frames: u64,
    pub audio_dropped: u64,
    pub audio_current_count: u64,
    pub audio_high_water_count: u64,
    pub writer_timeouts: u64,
    pub writer_errors: u64,
    pub transport_disconnects: u64,
    pub oldest_pending_ms: u64,
    pub oldest_pending_high_water_ms: u64,
    pub successful_write_bytes: u64,
    pub last_fault: Option<NorthboundFault>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NorthboundFault {
    pub code: String,
    pub uptime_ms: u64,
}
