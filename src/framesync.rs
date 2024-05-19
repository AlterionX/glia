use std::collections::HashMap;

use chrono::Utc;
use tokio::sync::RwLock;

use crate::netting::ClientId;

/// Stashes away the "more/less" bounds on the timeline.
///
/// Due to wraparound, this has a single discontinuous point. But the center is also a
/// discontinuous point, so we need 4 potential ranges to fully represent the thing.
#[derive(Debug, Clone)]
pub struct FrameWindowingRanges {
    pub center: u64,
}

impl FrameWindowingRanges {
    fn new(center: u64) -> Self {
        Self {
            center,
        }
    }

    fn signed_distance(&self, input: u64) -> i64 {
        let data = input.wrapping_sub(self.center);
        if data > u64::MAX / 2 {
            // This is the realm of negative distances.
            -((u64::MAX - data) as i64)
        } else {
            data as i64
        }
    }

    fn is_wraparound(&self, input: u64) -> bool {
        input.wrapping_sub(self.center) > input
    }
}

pub struct FrameSync {
    historical_framesyncs: HashMap<ClientId, (chrono::DateTime<chrono::Utc>, u64)>,
    range_cache: RwLock<Option<FrameWindowingRanges>>,
}

impl FrameSync {
    const FRAME_MILLISECONDS: u16 = 1000 / super::TARGET_FPS;
    /// We'll tolerate ~ 2 seconds of being ahead -- that's (currently) around how long it
    /// takes to make two round trips around the globe.
    const PEER_LAG_TOLERANCE: u16 = 2000 / Self::FRAME_MILLISECONDS;
    /// This is the bottom window of frames we're expecting from across the network. We won't have
    /// an upper limit, since the upper limit doesn't really exist.
    const VALIDITY_WINDOW_LOWER_DISTANCE: i64 =
        i64::MAX / 2;
        // Self::PEER_LAG_TOLERANCE as i64 * 2;
    const VALIDITY_WINDOW_UPPER_DISTANCE: i64 = 9999;
    /// We'll allow old data up to two times the lookahead tolerance. This should let us get away
    /// with using u16s as the frame integer type, but I'm lazy.
    const DATA_AGE_LIMIT: chrono::TimeDelta = chrono::TimeDelta::milliseconds(Self::PEER_LAG_TOLERANCE as i64 * 1000000);

    pub fn new() -> Self {
        Self {
            historical_framesyncs: HashMap::new(),
            range_cache: RwLock::new(None),
        }
    }

    /// To bust cache, set range_cache to None.
    async fn get_ranges(&self, center: u64) -> FrameWindowingRanges {
        let mut cache = self.range_cache.write().await;
        let data = match *cache {
            Some(ref r) if r.center == center => r.clone(),
            _ => {
                let r = FrameWindowingRanges::new(center);
                *cache = Some(r.clone());
                r
            },
        };
        data
    }

    pub fn record_framesync(&mut self, peer: ClientId, frame: u64) {
        let entry = self.historical_framesyncs.entry(peer).or_insert_with(|| (Utc::now(), frame));
        // Only update if frame advances the value.
        if entry.1 < frame {
            *entry = (Utc::now(), frame);
        }
    }

    pub async fn calculate_delay(&self, position: u64) -> Option<chrono::TimeDelta> {
        let earliest_allowed_timestamp = Utc::now() - Self::DATA_AGE_LIMIT;
        // First find the "earliest" frame synchronized. We'll use this as the limiting factor.
        let mut limiter = None;
        for (&peer, &(update_timestamp, frame)) in self.historical_framesyncs.iter() {
            if update_timestamp < earliest_allowed_timestamp {
                continue;
            }
            let Some((_wraparound, signed_distance)) = self.signed_distance(position, frame).await else {
                // Since this is "too far" aka the peer is problematic, we'll just ignore them.
                trc::warn!("FRAMESYNC-PEER-IGNORE peer={peer:?} frame={frame:?}");
                continue;
            };
            trc::trace!("FRAMESYNC-DIST peer={peer:?} frame={frame:?} distance={signed_distance:?}");
            let Some(prev) = limiter else {
                trc::trace!("FRAMESYNC-LIMITER-UPDATE peer={peer:?} frame={frame:?} distance={signed_distance:?}");
                limiter = Some((peer, frame, signed_distance));
                continue;
            };
            if signed_distance < prev.2 {
                trc::trace!("FRAMESYNC-LIMITER-UPDATE peer={peer:?} frame={frame:?} distance={signed_distance:?}");
                limiter = Some((peer, frame, signed_distance));
            } else {
                limiter = Some(prev);
            }
        }
        trc::info!("FRAMESYNC-LIMITER-RECORD center={position:?} limiter={limiter:?}");
        // If we don't have an earliest frame, assume we're alone and proceed as if we can
        // continue.
        let (_, limiting_frame, _) = limiter?;

        self.required_delay(position, limiting_frame).await
    }

    /// Returns Some((is_wraparound, signed_distance)).
    /// Returns None if this value should be ignored.
    async fn signed_distance(&self, position: u64, input: u64) -> Option<(bool, i64)> {
        let r = self.get_ranges(position).await;
        let distance = r.signed_distance(input);
        trc::info!("FRAMESYNC-SIGNED-DISTANCE position={position:?} input={input:?}");
        if distance < -Self::VALIDITY_WINDOW_LOWER_DISTANCE || distance > Self::VALIDITY_WINDOW_UPPER_DISTANCE {
            return None;
        }
        Some((r.is_wraparound(input), distance))
    }

    async fn required_delay(&self, position: u64, input: u64) -> Option<chrono::TimeDelta> {
        let r = self.get_ranges(position).await;
        let distance = r.signed_distance(input);
        if distance > -(Self::PEER_LAG_TOLERANCE as i64) {
            return None;
        }

        // We don't want to wait too long in case the network catchs up to us -- hence the min(20).
        let frames_to_wait = (distance + (Self::PEER_LAG_TOLERANCE as i64)).min(20);
        Some(chrono::TimeDelta::milliseconds(Self::FRAME_MILLISECONDS as i64 * frames_to_wait as i64))
    }
}
