#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportLane {
    Control,
    RealtimeInput,
    Bulk,
}
