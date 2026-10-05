//! Runtime configuration is resolved once per turn; an in-flight turn keeps its snapshot.

use crate::{CompactionSettings, ParallelSettings, microcompact::MicrocompactSettings};

#[derive(Debug, Clone, Default)]
pub struct DriverSettings {
    pub compaction: CompactionSettings,
    pub microcompact: MicrocompactSettings,
    pub parallel: ParallelSettings,
}

/// The host supplies live configuration without exposing its settings store to the loop.
pub trait DriverSettingsSource: Send + Sync {
    fn settings(&self) -> DriverSettings;
}
