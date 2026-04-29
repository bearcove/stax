use std::time::Duration;

const SLOW_CHANNEL_SEND: Duration = Duration::from_millis(10);
const SLOW_REQUEST: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug)]
pub struct VoxObserverLogger {
    component: &'static str,
    surface: &'static str,
    pid: Option<u32>,
}

impl VoxObserverLogger {
    pub const fn new(component: &'static str, surface: &'static str) -> Self {
        Self {
            component,
            surface,
            pid: None,
        }
    }

    pub const fn with_pid(mut self, pid: u32) -> Self {
        self.pid = Some(pid);
        self
    }
}

impl vox::VoxObserver for VoxObserverLogger {
    fn rpc_event(&self, event: vox::RpcEvent) {
        match event {
            vox::RpcEvent::Started {
                service,
                method,
                method_id,
                ..
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    service = ?service,
                    method = ?method,
                    method_id = ?method_id,
                    "vox rpc started"
                );
            }
            vox::RpcEvent::Finished {
                service,
                method,
                method_id,
                outcome,
                elapsed,
                ..
            } => {
                if outcome != vox::RpcOutcome::Ok || elapsed >= SLOW_REQUEST {
                    tracing::info!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        service = ?service,
                        method = ?method,
                        method_id = ?method_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox rpc finished"
                    );
                } else {
                    tracing::trace!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        service = ?service,
                        method = ?method,
                        method_id = ?method_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox rpc finished"
                    );
                }
            }
        }
    }

    fn channel_event(&self, event: vox::ChannelEvent) {
        match event {
            vox::ChannelEvent::Opened {
                channel_id,
                direction,
                initial_credit,
            } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    direction = ?direction,
                    initial_credit,
                    "vox channel opened"
                );
            }
            vox::ChannelEvent::SendWaitingForCredit { channel_id } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    "vox channel waiting for credit"
                );
            }
            vox::ChannelEvent::SendFinished {
                channel_id,
                outcome,
                elapsed,
            } => {
                if outcome != vox::ChannelSendOutcome::Sent || elapsed >= SLOW_CHANNEL_SEND {
                    tracing::info!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        channel_id = ?channel_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox channel send finished"
                    );
                } else {
                    tracing::trace!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        channel_id = ?channel_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox channel send finished"
                    );
                }
            }
            vox::ChannelEvent::TrySend {
                channel_id,
                outcome,
            } => {
                if outcome != vox::ChannelTrySendOutcome::Sent {
                    tracing::warn!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        channel_id = ?channel_id,
                        outcome = ?outcome,
                        "vox channel try_send failed"
                    );
                }
            }
            vox::ChannelEvent::Closed { channel_id, reason } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    reason = ?reason,
                    "vox channel closed"
                );
            }
            vox::ChannelEvent::Reset { channel_id, reason } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    reason = ?reason,
                    "vox channel reset"
                );
            }
            vox::ChannelEvent::SendStarted { channel_id } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    "vox channel send started"
                );
            }
            vox::ChannelEvent::CreditGranted { channel_id, amount } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    amount,
                    "vox channel credit granted"
                );
            }
            vox::ChannelEvent::ItemReceived { channel_id } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    "vox channel item received"
                );
            }
            vox::ChannelEvent::ItemConsumed { channel_id } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    channel_id = ?channel_id,
                    "vox channel item consumed"
                );
            }
        }
    }

    fn driver_event(&self, event: vox::DriverEvent) {
        match event {
            vox::DriverEvent::ConnectionOpened { connection_id } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    "vox connection opened"
                );
            }
            vox::DriverEvent::ConnectionClosed {
                connection_id,
                reason,
            } => {
                tracing::info!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    reason = ?reason,
                    "vox connection closed"
                );
            }
            vox::DriverEvent::RequestFinished {
                connection_id,
                request_id,
                outcome,
                elapsed,
            } => {
                if outcome != vox::RpcOutcome::Ok || elapsed >= SLOW_REQUEST {
                    tracing::info!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?connection_id,
                        request_id = ?request_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox request finished"
                    );
                } else {
                    tracing::trace!(
                        component = self.component,
                        surface = self.surface,
                        pid = ?self.pid,
                        connection_id = ?connection_id,
                        request_id = ?request_id,
                        outcome = ?outcome,
                        elapsed = ?elapsed,
                        "vox request finished"
                    );
                }
            }
            vox::DriverEvent::OutboundQueueFull { connection_id } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    "vox outbound queue full"
                );
            }
            vox::DriverEvent::OutboundQueueClosed { connection_id } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    "vox outbound queue closed"
                );
            }
            vox::DriverEvent::DecodeError {
                connection_id,
                kind,
            } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    kind = ?kind,
                    "vox decode error"
                );
            }
            vox::DriverEvent::EncodeError {
                connection_id,
                kind,
            } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    kind = ?kind,
                    "vox encode error"
                );
            }
            vox::DriverEvent::ProtocolError {
                connection_id,
                kind,
            } => {
                tracing::warn!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    kind = ?kind,
                    "vox protocol error"
                );
            }
            vox::DriverEvent::RequestStarted {
                connection_id,
                request_id,
                method_id,
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    request_id = ?request_id,
                    method_id = ?method_id,
                    "vox request started"
                );
            }
            vox::DriverEvent::FrameRead {
                connection_id,
                bytes,
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    bytes,
                    "vox frame read"
                );
            }
            vox::DriverEvent::FrameWritten {
                connection_id,
                bytes,
            } => {
                tracing::trace!(
                    component = self.component,
                    surface = self.surface,
                    pid = ?self.pid,
                    connection_id = ?connection_id,
                    bytes,
                    "vox frame written"
                );
            }
        }
    }

    fn transport_event(&self, event: vox::TransportEvent) {
        tracing::trace!(
            component = self.component,
            surface = self.surface,
            pid = ?self.pid,
            event = ?event,
            "vox transport event"
        );
    }
}
