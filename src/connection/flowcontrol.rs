// Copyright (c) 2023 The TQUIC Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::time::Duration;
use std::time::Instant;

/// A flow control implementation that allows the size of the receive buffer to
/// be auto-tuned.
///
/// The basic idea is to start with relatively small initial window size, and
/// then grow the window as necessary. For simplicity, auto-tuning may increase
/// the window size, but never decreases (contrast with congestion control).
///
/// The ideal size of the window is one that is large enough that it can
/// encompass the bandwidth delay product (BDP) to the peer.
///
/// The algorithm will compare the interval between successive flow control
/// window updates to the smoothed RTT. If the flow control window is too small
/// to keep up with the BDP, there will be a window update each RTT.
/// Alternatively, when the window is sized to the ideal, window updates can be
/// expected to occur with frequency corresponding to more than the 1 RTT
/// indicative of blocking, but not too much more. The default target chosen for
/// auto-tuning corresponds to 2 RTTs.
#[derive(Default, Debug)]
pub struct FlowControl {
    /// Number of bytes consumed (cumulative).
    ///
    /// For a stream, this value is the number of bytes consumed by the application.
    ///
    /// For a connection, this value is the sum of the number of bytes consumed by
    /// the application for all streams.
    pub read_off: u64,

    /// Largest offset observed (cumulative).
    ///
    /// For a stream, this value is the largest offset of data received.
    ///
    /// For a connection, this value is the sum of the largest offset received for all streams.
    recv_off: u64,

    /// The maximum amount of data that can be received for a given connection or stream.
    max_data: u64,

    /// Receive window, used to update max_data.
    window: u64,

    /// Maximum receive window, used to specify maximum flow control window allowed to reach
    /// due to window autotuning.
    max_window: u64,

    /// Timestamp of the last update moment of max_data due to window autotuning.
    last_updated: Option<Instant>,
}

impl FlowControl {
    pub fn new(window: u64, max_window: u64) -> FlowControl {
        FlowControl {
            max_data: window,
            window,
            max_window,
            ..FlowControl::default()
        }
    }

    /// Get the current receive window size.
    pub fn window(&self) -> u64 {
        self.window
    }

    /// Get the current flow control limit.
    pub fn max_data(&self) -> u64 {
        self.max_data
    }

    /// Get the largest received offset observed.
    pub fn recv_off(&self) -> u64 {
        self.recv_off
    }

    /// Update the largest received offset observed.
    pub fn increase_recv_off(&mut self, delta: u64) {
        self.recv_off += delta;
    }

    /// Update the number of bytes consumed.
    pub fn increase_read_off(&mut self, delta: u64) {
        self.read_off += delta;
    }

    /// Check if we should send a MAX_DATA/MAX_STREAM_DATA frame to the peer.
    ///
    /// Return true if the available window is smaller than the half
    /// of the current window.
    pub fn should_send_max_data(&self) -> bool {
        let v = (self.max_data - self.read_off) * 2 < self.window;
        log::debug!(
            "~~~ should_send_max_data {}: max_data {} - read_off {} < window {} / 2",
            v,
            self.max_data,
            self.read_off,
            self.window
        );
        v
    }

    /// Get the next max_data limit which will be sent to the peer
    /// in a MAX_DATA/MAX_STREAM_DATA frame.
    pub fn max_data_next(&self) -> u64 {
        self.read_off + self.window
    }

    /// Apply the new max_data limit.
    pub fn update_max_data(&mut self, now: Instant) {
        self.max_data = self.max_data_next();
        self.last_updated = Some(now);
    }

    /// Adjust the window size automatically. If the last update
    /// is within 2 * srtt, increase the window size by 2, but
    /// not exceeding the max_window.
    pub fn autotune_window(&mut self, now: Instant, srtt: Duration) {
        if let Some(last_updated) = self.last_updated {
            if now - last_updated < srtt * 2 {
                self.window = std::cmp::min(self.window * 2, self.max_window);
            }
        }
    }

    /// Ensure that the lower bound of the window is equal to
    /// the given min_window.
    pub fn ensure_window_lower_bound(&mut self, min_window: u64) {
        self.window = std::cmp::max(self.window, min_window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fc_new() {
        let flow_control = FlowControl::new(100, 200);

        assert_eq!(flow_control.max_data(), 100);
        assert_eq!(flow_control.window(), 100);
        assert_eq!(flow_control.max_window, 200);
        assert_eq!(flow_control.read_off, 0);
        assert_eq!(flow_control.recv_off, 0);
        assert_eq!(flow_control.last_updated, None);
    }

    #[test]
    fn fc_increase_recv_off() {
        let mut fc = FlowControl::new(100, 200);

        for (delta, total) in [(10, 10), (20, 30), (30, 60)] {
            fc.increase_recv_off(delta);
            assert_eq!(fc.recv_off, total);
        }
    }

    #[test]
    fn fc_update_logic() {
        let mut fc = FlowControl::new(100, 200);

        for (read_delta, read_off, should_send, max_data_next) in [
            // 1. Initial state
            (0, 0, false, 100),
            // 2. Read 50 bytes
            // available window is 50 == window / 2, not need to send max_data,
            // max_data_next is 150 = read_off(50) + window(100)
            (50, 50, false, 150),
            // 3. Read 1 bytes
            // available window is 49 < window / 2, need to send max_data
            // max_data_next is 151 = read_off(51) + window(100)
            (1, 51, true, 151),
        ] {
            fc.increase_read_off(read_delta);
            assert_eq!(fc.read_off, read_off);
            assert_eq!(fc.should_send_max_data(), should_send);
            assert_eq!(fc.max_data_next(), max_data_next);
        }

        fc.update_max_data(Instant::now());
        assert_eq!(fc.max_data(), 151);
    }

    #[test]
    fn fc_autotune_window() {
        let window = 10;
        let max_window = 30;
        let now = Instant::now();
        let srtt = Duration::from_millis(100);
        let mut fc = FlowControl::new(window, max_window);

        // 1. Read 6 bytes, available window is 4 < window / 2, need to send max_data.
        let read_off = 6;
        fc.increase_read_off(read_off);
        assert_eq!(fc.should_send_max_data(), true);

        // max_data_next = read_off(6) + window(10) = 16
        let max_data_next = fc.max_data_next();
        assert_eq!(max_data_next, read_off + fc.window);

        // 2. Apply the new max_data limit(16), last_updated is set to now.
        fc.update_max_data(now);
        assert_eq!(fc.max_data(), max_data_next);

        // 3. Last update is within 2 * srtt, window size should be doubled.
        fc.autotune_window(now + srtt / 2, srtt);
        // Window auto-tuned to 20
        assert_eq!(fc.window, window * 2);

        // 4. Read 5 byte, available window is 9 < window / 2, need to send max_data.
        let read_off_delta = 5;
        fc.increase_read_off(read_off_delta);
        assert_eq!(fc.should_send_max_data(), true);

        // max_data_next = read_off(11) + window(20) = 31
        let max_data_next = fc.max_data_next();
        assert_eq!(max_data_next, read_off + read_off_delta + fc.window);

        // 5. Apply the new max_data limit(31), last_updated is set to now.
        fc.update_max_data(now);
        assert_eq!(fc.max_data(), max_data_next);

        // 6. Last update is within 2 * srtt, window size should be doubled, but
        // max_window is 30, so window size should be 30.
        fc.autotune_window(now + srtt / 2, srtt);
        // Window auto-tuned to max_window(30)
        assert_eq!(fc.window, max_window);
    }

    #[test]
    fn fc_ensure_window_lower_bound() {
        let mut fc = FlowControl::new(10, 200);

        for (min_window, window) in [
            // min_window < window, unchanged
            (9, 10),
            // min_window == window, unchanged
            (10, 10),
            // min_window > window, updated
            (11, 11),
        ] {
            fc.ensure_window_lower_bound(min_window);
            assert_eq!(fc.window(), window);
        }
    }
}
