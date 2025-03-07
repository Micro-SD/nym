use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{RwLock, RwLockReadGuard};

use super::ShutdownListener;

// convenience aliases
type PacketsMap = HashMap<String, u64>;
type PacketDataReceiver = mpsc::UnboundedReceiver<PacketEvent>;
type PacketDataSender = mpsc::UnboundedSender<PacketEvent>;

#[derive(Clone)]
pub(crate) struct SharedNodeStats {
    inner: Arc<RwLock<NodeStats>>,
}

impl SharedNodeStats {
    pub(crate) fn new() -> Self {
        let now = SystemTime::now();
        SharedNodeStats {
            inner: Arc::new(RwLock::new(NodeStats {
                update_time: now,
                previous_update_time: now,
                packets_received_since_startup: 0,
                packets_sent_since_startup: HashMap::new(),
                packets_explicitly_dropped_since_startup: HashMap::new(),
                packets_received_since_last_update: 0,
                packets_sent_since_last_update: HashMap::new(),
                packets_explicitly_dropped_since_last_update: HashMap::new(),
            })),
        }
    }

    pub(crate) async fn update(
        &self,
        new_received: u64,
        new_sent: PacketsMap,
        new_dropped: PacketsMap,
    ) {
        let mut guard = self.inner.write().await;
        let snapshot_time = SystemTime::now();

        guard.previous_update_time = guard.update_time;
        guard.update_time = snapshot_time;

        guard.packets_received_since_startup += new_received;
        for (mix, count) in &new_sent {
            *guard
                .packets_sent_since_startup
                .entry(mix.clone())
                .or_insert(0) += *count;
        }

        for (mix, count) in &new_dropped {
            *guard
                .packets_explicitly_dropped_since_last_update
                .entry(mix.clone())
                .or_insert(0) += *count;
        }

        guard.packets_received_since_last_update = new_received;
        guard.packets_sent_since_last_update = new_sent;
        guard.packets_explicitly_dropped_since_last_update = new_dropped;
    }

    pub(crate) async fn clone_data(&self) -> NodeStats {
        self.inner.read().await.clone()
    }

    async fn read(&self) -> RwLockReadGuard<'_, NodeStats> {
        self.inner.read().await
    }
}

#[derive(Serialize, Clone)]
pub(crate) struct NodeStats {
    #[serde(serialize_with = "humantime_serde::serialize")]
    update_time: SystemTime,

    #[serde(serialize_with = "humantime_serde::serialize")]
    previous_update_time: SystemTime,

    packets_received_since_startup: u64,

    // note: sent does not imply forwarded. We don't know if it was delivered successfully
    packets_sent_since_startup: PacketsMap,

    // we know for sure we dropped packets to those destinations
    packets_explicitly_dropped_since_startup: PacketsMap,

    packets_received_since_last_update: u64,

    // note: sent does not imply forwarded. We don't know if it was delivered successfully
    packets_sent_since_last_update: PacketsMap,

    // we know for sure we dropped packets to those destinations
    packets_explicitly_dropped_since_last_update: PacketsMap,
}

impl NodeStats {
    pub(crate) fn simplify(&self) -> NodeStatsSimple {
        NodeStatsSimple {
            update_time: self.update_time,
            previous_update_time: self.previous_update_time,
            packets_received_since_startup: self.packets_received_since_startup,
            packets_sent_since_startup: self.packets_sent_since_startup.values().sum(),
            packets_explicitly_dropped_since_startup: self
                .packets_explicitly_dropped_since_startup
                .values()
                .sum(),
            packets_received_since_last_update: self.packets_received_since_last_update,
            packets_sent_since_last_update: self.packets_sent_since_last_update.values().sum(),
            packets_explicitly_dropped_since_last_update: self
                .packets_explicitly_dropped_since_last_update
                .values()
                .sum(),
        }
    }
}

#[derive(Serialize, Clone)]
pub(crate) struct NodeStatsSimple {
    #[serde(serialize_with = "humantime_serde::serialize")]
    update_time: SystemTime,

    #[serde(serialize_with = "humantime_serde::serialize")]
    previous_update_time: SystemTime,

    packets_received_since_startup: u64,

    // note: sent does not imply forwarded. We don't know if it was delivered successfully
    packets_sent_since_startup: u64,

    // we know for sure we dropped those packets
    packets_explicitly_dropped_since_startup: u64,

    packets_received_since_last_update: u64,

    // note: sent does not imply forwarded. We don't know if it was delivered successfully
    packets_sent_since_last_update: u64,

    // we know for sure we dropped those packets
    packets_explicitly_dropped_since_last_update: u64,
}

pub(crate) enum PacketEvent {
    Sent(String),
    Received,
    Dropped(String),
}

#[derive(Debug, Clone)]
struct CurrentPacketData {
    inner: Arc<PacketDataInner>,
}

#[derive(Debug)]
struct PacketDataInner {
    received: AtomicU64,
    sent: Mutex<PacketsMap>,
    dropped: Mutex<PacketsMap>,
}

impl CurrentPacketData {
    pub(crate) fn new() -> Self {
        CurrentPacketData {
            inner: Arc::new(PacketDataInner {
                received: AtomicU64::new(0),
                sent: Mutex::new(HashMap::new()),
                dropped: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn increment_received(&self) {
        self.inner.received.fetch_add(1, Ordering::SeqCst);
    }

    async fn increment_sent(&self, destination: String) {
        let mut unlocked = self.inner.sent.lock().await;
        let receiver_count = unlocked.entry(destination).or_insert(0);
        *receiver_count += 1;
    }

    async fn increment_dropped(&self, destination: String) {
        let mut unlocked = self.inner.dropped.lock().await;
        let dropped_count = unlocked.entry(destination).or_insert(0);
        *dropped_count += 1;
    }

    async fn acquire_and_reset(&self) -> (u64, PacketsMap, PacketsMap) {
        let mut unlocked_sent = self.inner.sent.lock().await;
        let mut unlocked_dropped = self.inner.dropped.lock().await;
        let received = self.inner.received.swap(0, Ordering::SeqCst);

        let sent = std::mem::take(unlocked_sent.deref_mut());
        let dropped = std::mem::take(unlocked_dropped.deref_mut());

        (received, sent, dropped)
    }
}

// Worker that listens to a channel and updates the shared current packet data
struct UpdateHandler {
    current_data: CurrentPacketData,
    update_receiver: PacketDataReceiver,
    shutdown: ShutdownListener,
}

impl UpdateHandler {
    fn new(
        current_data: CurrentPacketData,
        update_receiver: PacketDataReceiver,
        shutdown: ShutdownListener,
    ) -> Self {
        UpdateHandler {
            current_data,
            update_receiver,
            shutdown,
        }
    }

    async fn run(&mut self) {
        log::trace!("Starting UpdateHandler");
        while !self.shutdown.is_shutdown() {
            tokio::select! {
                Some(packet_data) = self.update_receiver.next() => {
                    match packet_data {
                        PacketEvent::Received => self.current_data.increment_received(),
                        PacketEvent::Sent(destination) => {
                            self.current_data.increment_sent(destination).await
                        }
                        PacketEvent::Dropped(destination) => {
                            self.current_data.increment_dropped(destination).await
                        }
                    }
                }
                _ = self.shutdown.recv() => {
                    log::trace!("UpdateHandler: Received shutdown");
                    break;
                }
            }
        }

        log::trace!("UpdateHandler: Exiting");
    }
}

// Channel to report statistics
#[derive(Clone)]
pub struct UpdateSender(PacketDataSender);

impl UpdateSender {
    pub(crate) fn new(update_sender: PacketDataSender) -> Self {
        UpdateSender(update_sender)
    }

    pub(crate) fn report_sent(&self, destination: String) {
        // in unbounded_send() failed it means that the receiver channel was disconnected
        // and hence something weird must have happened without a way of recovering
        self.0
            .unbounded_send(PacketEvent::Sent(destination))
            .unwrap()
    }

    // TODO: in the future this could be slightly optimised to get rid of the channel
    // in favour of incrementing value directly
    pub(crate) fn report_received(&self) {
        // in unbounded_send() failed it means that the receiver channel was disconnected
        // and hence something weird must have happened without a way of recovering
        self.0.unbounded_send(PacketEvent::Received).unwrap()
    }

    pub(crate) fn report_dropped(&self, destination: String) {
        // in unbounded_send() failed it means that the receiver channel was disconnected
        // and hence something weird must have happened without a way of recovering
        self.0
            .unbounded_send(PacketEvent::Dropped(destination))
            .unwrap()
    }
}

// Worker that periodically updates the shared node stats from the current packet data buffer that
// the `UpdateHandler` updates.
struct StatsUpdater {
    updating_delay: Duration,
    current_packet_data: CurrentPacketData,
    current_stats: SharedNodeStats,
    shutdown: ShutdownListener,
}

impl StatsUpdater {
    fn new(
        updating_delay: Duration,
        current_packet_data: CurrentPacketData,
        current_stats: SharedNodeStats,
        shutdown: ShutdownListener,
    ) -> Self {
        StatsUpdater {
            updating_delay,
            current_packet_data,
            current_stats,
            shutdown,
        }
    }

    async fn update_stats(&self) {
        // grab new data since last update
        let (received, sent, dropped) = self.current_packet_data.acquire_and_reset().await;
        self.current_stats.update(received, sent, dropped).await;
    }

    async fn run(&mut self) {
        while !self.shutdown.is_shutdown() {
            tokio::select! {
                _ = tokio::time::sleep(self.updating_delay) => self.update_stats().await,
                _ = self.shutdown.recv() => {
                    log::trace!("StatsUpdater: Received shutdown");
                }
            }
        }
        log::trace!("StatsUpdater: Exiting");
    }
}

// TODO: question: should this data still be logged to the console or should we perhaps remove it
// since we have the http endpoint now?
struct PacketStatsConsoleLogger {
    logging_delay: Duration,
    stats: SharedNodeStats,
    shutdown: ShutdownListener,
}

impl PacketStatsConsoleLogger {
    fn new(logging_delay: Duration, stats: SharedNodeStats, shutdown: ShutdownListener) -> Self {
        PacketStatsConsoleLogger {
            logging_delay,
            stats,
            shutdown,
        }
    }

    async fn log_running_stats(&mut self) {
        let stats = self.stats.read().await;

        // it's super unlikely this will ever fail, but anything involving time is super weird
        // so let's just guard against it
        if let Ok(time_difference) = stats.update_time.duration_since(stats.previous_update_time) {
            // we honestly don't care if it was 30.000828427s or 30.002461449s, 30s is enough
            let difference_secs = time_difference.as_secs();

            info!(
                "Since startup mixed {} packets! ({} in last {} seconds)",
                stats.packets_sent_since_startup.values().sum::<u64>(),
                stats.packets_sent_since_last_update.values().sum::<u64>(),
                difference_secs,
            );
            if !stats.packets_explicitly_dropped_since_startup.is_empty() {
                info!(
                    "Since startup dropped {} packets! ({} in last {} seconds)",
                    stats
                        .packets_explicitly_dropped_since_startup
                        .values()
                        .sum::<u64>(),
                    stats
                        .packets_explicitly_dropped_since_last_update
                        .values()
                        .sum::<u64>(),
                    difference_secs,
                );
            }

            debug!(
                "Since startup received {} packets ({} in last {} seconds)",
                stats.packets_received_since_startup,
                stats.packets_received_since_last_update,
                difference_secs,
            );
            trace!(
                "Since startup sent packets to the following: \n{:#?} \n And in last {} seconds: {:#?})",
                stats.packets_sent_since_startup,
                difference_secs,
                stats.packets_sent_since_last_update
            );
        } else {
            info!(
                "Since startup mixed {} packets!",
                stats.packets_sent_since_startup.values().sum::<u64>(),
            );
            if !stats.packets_explicitly_dropped_since_startup.is_empty() {
                info!(
                    "Since startup dropped {} packets!",
                    stats
                        .packets_explicitly_dropped_since_startup
                        .values()
                        .sum::<u64>(),
                );
            }

            debug!(
                "Since startup received {} packets",
                stats.packets_received_since_startup
            );
            trace!(
                "Since startup sent packets to the following: \n{:#?}",
                stats.packets_sent_since_startup
            );
        }
    }

    async fn run(&mut self) {
        log::trace!("Starting PacketStatsConsoleLogger");
        while !self.shutdown.is_shutdown() {
            tokio::select! {
                _ = tokio::time::sleep(self.logging_delay) => self.log_running_stats().await,
                _ = self.shutdown.recv() => {
                    log::trace!("PacketStatsConsoleLogger: Received shutdown");
                }
            };
        }
        log::trace!("PacketStatsConsoleLogger: Exiting");
    }
}

// basically an easy single entry point to start all of the required tasks
pub struct Controller {
    /// Responsible for handling data coming from UpdateSender
    update_handler: UpdateHandler,

    /// Wrapper around channel sending information about new packet being received or sent
    update_sender: UpdateSender,

    /// Responsible for logging stats to the console at given interval
    console_logger: PacketStatsConsoleLogger,

    /// Responsible for updating stats at given interval
    stats_updater: StatsUpdater,

    /// Pointer to the current node stats
    node_stats: SharedNodeStats,
}

impl Controller {
    pub(crate) fn new(
        logging_delay: Duration,
        stats_updating_delay: Duration,
        shutdown: ShutdownListener,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        let shared_packet_data = CurrentPacketData::new();
        let shared_node_stats = SharedNodeStats::new();

        Controller {
            update_handler: UpdateHandler::new(
                shared_packet_data.clone(),
                receiver,
                shutdown.clone(),
            ),
            update_sender: UpdateSender::new(sender),
            console_logger: PacketStatsConsoleLogger::new(
                logging_delay,
                shared_node_stats.clone(),
                shutdown.clone(),
            ),
            stats_updater: StatsUpdater::new(
                stats_updating_delay,
                shared_packet_data,
                shared_node_stats.clone(),
                shutdown,
            ),
            node_stats: shared_node_stats,
        }
    }

    pub(crate) fn get_node_stats_data_pointer(&self) -> SharedNodeStats {
        SharedNodeStats {
            inner: Arc::clone(&self.node_stats.inner),
        }
    }

    // reporter is how node is going to be accessing the metrics data
    pub(crate) fn start(self) -> UpdateSender {
        // move out of self
        let mut update_handler = self.update_handler;
        let mut stats_updater = self.stats_updater;
        let mut console_logger = self.console_logger;

        tokio::spawn(async move { update_handler.run().await });
        tokio::spawn(async move { stats_updater.run().await });
        tokio::spawn(async move { console_logger.run().await });

        self.update_sender
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use task::ShutdownNotifier;

    #[tokio::test]
    async fn node_stats_reported_are_received() {
        let logging_delay = Duration::from_millis(20);
        let stats_updating_delay = Duration::from_millis(10);
        let shutdown = ShutdownNotifier::default();
        let node_stats_controller =
            Controller::new(logging_delay, stats_updating_delay, shutdown.subscribe());

        let node_stats_pointer = node_stats_controller.get_node_stats_data_pointer();
        let update_sender = node_stats_controller.start();
        tokio::time::pause();

        // Pass input
        update_sender.report_sent("foo".to_string());
        update_sender.report_sent("foo".to_string());
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        // Get output (stats)
        let stats = node_stats_pointer.read().await;
        assert_eq!(&stats.packets_sent_since_startup.get("foo"), &Some(&2u64));
        assert_eq!(&stats.packets_sent_since_startup.len(), &1);
        assert_eq!(
            &stats.packets_sent_since_last_update.get("foo"),
            &Some(&2u64)
        );
        assert_eq!(&stats.packets_sent_since_last_update.len(), &1);
        assert_eq!(&stats.packets_received_since_startup, &0u64);
        assert!(&stats.packets_explicitly_dropped_since_startup.is_empty());
    }
}
