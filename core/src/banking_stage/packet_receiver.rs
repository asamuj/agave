use {
    super::{
        immutable_deserialized_packet::ImmutableDeserializedPacket,
        leader_slot_metrics::LeaderSlotMetricsTracker,
        packet_deserializer::{PacketDeserializer, ReceivePacketResults},
        vote_storage::VoteStorage,
        BankingStageStats,
    },
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    crossbeam_channel::RecvTimeoutError,
    solana_measure::{measure::Measure, measure_us},
    solana_sdk::{saturating_add_assign, timing::timestamp},
    std::{sync::atomic::Ordering, time::Duration},
};

pub struct PacketReceiver {
    id: u32,
    packet_deserializer: PacketDeserializer,
}

impl PacketReceiver {
    pub fn new(id: u32, banking_packet_receiver: BankingPacketReceiver) -> Self {
        Self {
            id,
            packet_deserializer: PacketDeserializer::new(banking_packet_receiver),
        }
    }

    /// Receive incoming packets, push into unprocessed buffer with packet indexes
    pub fn receive_and_buffer_packets(
        &mut self,
        vote_storage: &mut VoteStorage,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) -> Result<(), RecvTimeoutError> {
        let (result, recv_time_us) = measure_us!({
            let recv_timeout = Self::get_receive_timeout(vote_storage);
            let mut recv_and_buffer_measure = Measure::start("recv_and_buffer");
            self.packet_deserializer
                .receive_packets(recv_timeout, vote_storage.max_receive_size(), |packet| {
                    packet.check_insufficent_compute_unit_limit()?;
                    packet.check_excessive_precompiles()?;
                    Ok(packet)
                })
                // Consumes results if Ok, otherwise we keep the Err
                .map(|receive_packet_results| {
                    self.buffer_packets(
                        receive_packet_results,
                        vote_storage,
                        banking_stage_stats,
                        slot_metrics_tracker,
                    );
                    recv_and_buffer_measure.stop();

                    // Only incremented if packets are received
                    banking_stage_stats
                        .receive_and_buffer_packets_elapsed
                        .fetch_add(recv_and_buffer_measure.as_us(), Ordering::Relaxed);
                })
        });

        slot_metrics_tracker.increment_receive_and_buffer_packets_us(recv_time_us);

        result
    }

    fn get_receive_timeout(vote_storage: &VoteStorage) -> Duration {
        // Gossip thread (does not process) should not continuously receive with 0 duration.
        // This can cause the thread to run at 100% CPU because it is continuously polling.
        if !vote_storage.should_not_process() && !vote_storage.is_empty() {
            // If there are buffered packets, run the equivalent of try_recv to try reading more
            // packets. This prevents starving BankingStage::consume_buffered_packets due to
            // buffered_packet_batches containing transactions that exceed the cost model for
            // the current bank.
            Duration::from_millis(0)
        } else {
            // Default wait time
            Duration::from_millis(100)
        }
    }

    fn buffer_packets(
        &self,
        ReceivePacketResults {
            deserialized_packets,
            packet_stats,
        }: ReceivePacketResults,
        vote_storage: &mut VoteStorage,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        let packet_count = deserialized_packets.len();
        debug!("@{:?} txs: {} id: {}", timestamp(), packet_count, self.id);

        slot_metrics_tracker.increment_received_packet_counts(packet_stats);

        let mut dropped_packets_count = 0;
        let mut newly_buffered_packets_count = 0;
        let mut newly_buffered_forwarded_packets_count = 0;
        Self::push_unprocessed(
            vote_storage,
            deserialized_packets,
            &mut dropped_packets_count,
            &mut newly_buffered_packets_count,
            &mut newly_buffered_forwarded_packets_count,
            banking_stage_stats,
            slot_metrics_tracker,
        );

        banking_stage_stats
            .receive_and_buffer_packets_count
            .fetch_add(packet_count, Ordering::Relaxed);
        banking_stage_stats
            .dropped_packets_count
            .fetch_add(dropped_packets_count, Ordering::Relaxed);
        banking_stage_stats
            .newly_buffered_packets_count
            .fetch_add(newly_buffered_packets_count, Ordering::Relaxed);
        banking_stage_stats
            .current_buffered_packets_count
            .swap(vote_storage.len(), Ordering::Relaxed);
    }

    fn push_unprocessed(
        vote_storage: &mut VoteStorage,
        deserialized_packets: Vec<ImmutableDeserializedPacket>,
        dropped_packets_count: &mut usize,
        newly_buffered_packets_count: &mut usize,
        newly_buffered_forwarded_packets_count: &mut usize,
        banking_stage_stats: &mut BankingStageStats,
        slot_metrics_tracker: &mut LeaderSlotMetricsTracker,
    ) {
        if !deserialized_packets.is_empty() {
            let _ = banking_stage_stats
                .batch_packet_indexes_len
                .increment(deserialized_packets.len() as u64);

            *newly_buffered_packets_count += deserialized_packets.len();
            *newly_buffered_forwarded_packets_count += deserialized_packets
                .iter()
                .filter(|p| p.forwarded())
                .count();
            slot_metrics_tracker
                .increment_newly_buffered_packets_count(deserialized_packets.len() as u64);

            let vote_batch_insertion_metrics = vote_storage.insert_batch(deserialized_packets);
            slot_metrics_tracker
                .accumulate_vote_batch_insertion_metrics(&vote_batch_insertion_metrics);
            saturating_add_assign!(
                *dropped_packets_count,
                vote_batch_insertion_metrics.total_dropped_packets()
            );
        }
    }
}
