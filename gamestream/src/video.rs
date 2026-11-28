//! Video packets to frames. A frame arrives as one to four blocks of packets,
//! each block followed by parity packets that rebuild lost data packets. A
//! frame that loses more than its parity covers is dropped, and the stream
//! continues from the next key frame.

/// One encoded frame: an Annex B access unit for H.264 and HEVC.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub data: Vec<u8>,
    pub key: bool,
    pub index: u32,
    /// Capture time on the host, in 90 kHz ticks.
    pub timestamp: u32,
    /// Time the host took from capture to send, in tenths of a millisecond.
    pub host_latency: u16,
}

const HEADER: usize = 32;
const FRAME_HEADER: usize = 8;
const KEY_FRAME: u8 = 2;
/// The header bytes the host's parity covers: flags, extra flags and the
/// constant that follows them. They go in front of the payload in a shard, so
/// a rebuilt packet can be checked.
const COVERED: std::ops::Range<usize> = 24..27;
const PICTURE_DATA: u8 = 1;
const KNOWN_FLAGS: u8 = 7;
const MULTI_FEC: u8 = 0x10;
/// The code has no more shards than this; a larger block has no usable parity.
const MOST_SHARDS: usize = 255;

/// The packets of one block: its data shards, then its parity shards.
struct Block {
    frame: u32,
    index: u8,
    data: usize,
    /// Empty until the first packet of the block arrives.
    shards: Vec<Option<Vec<u8>>>,
    shard_length: usize,
    received: usize,
    missing_data: usize,
}

impl Block {
    fn new(frame: u32, index: u8) -> Self {
        Self { frame, index, data: 0, shards: Vec::new(), shard_length: 0, received: 0, missing_data: 0 }
    }

    /// Whether the data shards are all there, rebuilt from parity if needed.
    /// `None` while packets are still due.
    fn complete(&mut self) -> Option<bool> {
        if self.missing_data == 0 { return Some(true); }
        if self.received < self.data { return None; }
        let rebuilt: Vec<usize> = (0..self.data).filter(|&index| self.shards[index].is_none()).collect();
        if !crate::fec::recover(&mut self.shards, self.data) { return Some(false); }
        // A rebuilt packet that does not look like a data packet shows that a
        // received packet was damaged.
        Some(rebuilt.into_iter().all(|index| {
            let shard = self.shards[index].as_ref().unwrap();
            shard[0] & PICTURE_DATA != 0 && shard[0] & !KNOWN_FLAGS == 0 && shard[2] == MULTI_FEC
        }))
    }
}

pub(crate) struct Assembler {
    /// The last frame finished or given up.
    done: u32,
    /// The block being collected.
    block: Option<Block>,
    /// The finished blocks of the current frame.
    data: Vec<u8>,
    timestamp: u32,
    /// After a loss every frame up to the next key frame is useless.
    waiting_for_key: bool,
}

impl Assembler {
    pub(crate) fn new() -> Self {
        Self { done: 0, block: None, data: Vec::new(), timestamp: 0, waiting_for_key: true }
    }

    /// Whether nothing can be shown until the host sends a key frame. The
    /// caller keeps asking the host for one while this holds.
    pub(crate) fn needs_key_frame(&self) -> bool {
        self.waiting_for_key
    }

    /// The host has sent nothing for a while. A frame still missing packets
    /// will not get them, and a host showing a still picture may send no next
    /// frame to reveal the loss: the frame is lost now.
    pub(crate) fn idle(&mut self) {
        if let Some(frame) = self.block.as_ref().map(|block| block.frame) { self.give_up(frame); }
    }

    fn give_up(&mut self, frame: u32) {
        self.done = self.done.max(frame);
        self.block = None;
        self.data.clear();
        self.waiting_for_key = true;
    }

    /// Takes one datagram and returns the frame it completes, if any.
    pub(crate) fn push(&mut self, datagram: &[u8]) -> Option<Frame> {
        if datagram.len() <= HEADER { return None; }
        let word = |at: usize| u32::from_le_bytes(datagram[at..at + 4].try_into().unwrap());
        let frame = word(20);
        let (block, last_block) = ((datagram[27] >> 4) & 3, (datagram[27] >> 6) & 3);
        let (index, data) = (((word(28) >> 12) & 0x3ff) as usize, (word(28) >> 22) as usize);
        let parity = (data * ((word(28) >> 4) & 0xff) as usize).div_ceil(100);
        let parity = if data + parity > MOST_SHARDS { 0 } else { parity };
        if index >= data + parity || frame <= self.done { return None; }

        if let Some(current) = &self.block && (current.frame, current.index) != (frame, block) {
            if current.frame == frame && block < current.index { return None; }
            // The host has moved on while packets of this block are missing.
            let same_frame = current.frame == frame;
            self.give_up(current.frame);
            if same_frame { return None; }
        }
        if self.block.is_none() {
            if frame > self.done + 1 || block != 0 {
                // Whole frames, or the first block of this one, never arrived.
                self.give_up(if block != 0 { frame } else { frame - 1 });
                if block != 0 { return None; }
            }
            self.timestamp = u32::from_be_bytes(datagram[4..8].try_into().unwrap());
            self.block = Some(Block::new(frame, 0));
        }
        let current = self.block.as_mut().unwrap();
        if current.shards.is_empty() {
            current.data = data;
            current.missing_data = data;
            current.shards.resize(data + parity, None);
            current.shard_length = COVERED.len() + datagram.len() - HEADER;
        }
        if (current.data, current.shards.len()) != (data, data + parity) { return None; }
        if current.shards[index].is_none() {
            let mut shard = Vec::with_capacity(current.shard_length);
            shard.extend_from_slice(&datagram[COVERED]);
            shard.extend_from_slice(&datagram[HEADER..]);
            shard.resize(current.shard_length, 0);
            current.shards[index] = Some(shard);
            current.received += 1;
            if index < data { current.missing_data -= 1; }
        }
        if !current.complete()? {
            self.give_up(frame);
            return None;
        }
        let packet_payload = current.shard_length - COVERED.len();
        for shard in current.shards.drain(..).take(data) { self.data.extend_from_slice(&shard.unwrap()[COVERED.len()..]); }
        if block < last_block {
            *current = Block::new(frame, block + 1);
            return None;
        }

        self.block = None;
        self.done = frame;
        let data = std::mem::take(&mut self.data);
        // A short frame header: type, host latency, bytes used in the last packet.
        let key = data.get(3) == Some(&KEY_FRAME);
        let used = data.get(4..6).map_or(0, |used| u16::from_le_bytes([used[0], used[1]]) as usize);
        let padding = if (1..=packet_payload).contains(&used) { packet_payload - used } else { 0 };
        // A header that claims less than the frame header holds is damaged.
        let Some(bitstream) = data.get(FRAME_HEADER..data.len() - padding).filter(|_| data[0] == 1) else {
            self.give_up(frame);
            return None;
        };
        if self.waiting_for_key && !key { return None; }
        self.waiting_for_key = false;
        Some(Frame {
            data: bitstream.to_vec(),
            key,
            index: frame,
            timestamp: self.timestamp,
            host_latency: u16::from_le_bytes([data[1], data[2]]),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const PAYLOAD: usize = 16;

    /// The packets of one frame the way the host cuts them: `blocks` blocks,
    /// each followed by its parity packets (a fifth of the data, rounded up).
    pub(crate) fn packets(frame: u32, key: bool, bitstream: &[u8], blocks: usize) -> Vec<Vec<u8>> {
        let mut body = vec![1, 0x34, 0x12, if key { 2 } else { 1 }, 0, 0, 0, 0];
        body.extend_from_slice(bitstream);
        let used = match body.len() % PAYLOAD { 0 => PAYLOAD, used => used };
        body[4..6].copy_from_slice(&(used as u16).to_le_bytes());
        body.resize(body.len().div_ceil(PAYLOAD) * PAYLOAD, 0);
        let chunks: Vec<&[u8]> = body.chunks(PAYLOAD).collect();
        let per_block = chunks.len().div_ceil(blocks);
        let mut out = Vec::new();
        for (block, chunk) in chunks.chunks(per_block).enumerate() {
            let count = chunk.len();
            let header = |index: usize| {
                let mut packet = vec![0u8; HEADER];
                packet[0] = 0x90;
                packet[4..8].copy_from_slice(&(frame * 1500).to_be_bytes());
                packet[20..24].copy_from_slice(&frame.to_le_bytes());
                packet[27] = (block as u8) << 4 | ((blocks - 1) as u8) << 6;
                packet[28..32].copy_from_slice(&((20u32 << 4) | (index as u32) << 12 | (count as u32) << 22).to_le_bytes());
                packet
            };
            let data: Vec<Vec<u8>> = chunk.iter().enumerate().map(|(index, payload)| {
                let mut packet = header(index);
                packet[24] = PICTURE_DATA | if index == 0 { 4 } else { 0 } | if index + 1 == count { 2 } else { 0 };
                packet[26] = MULTI_FEC;
                packet.extend_from_slice(payload);
                packet
            }).collect();
            let shards: Vec<Vec<u8>> = data.iter().map(|packet| [&packet[COVERED], &packet[HEADER..]].concat()).collect();
            let parity = crate::fec::encode(&shards, (count * 20).div_ceil(100));
            out.extend(data);
            for (row, shard) in parity.iter().enumerate() {
                let mut packet = header(count + row);
                packet[COVERED].copy_from_slice(&shard[..COVERED.len()]);
                packet.extend_from_slice(&shard[COVERED.len()..]);
                out.push(packet);
            }
        }
        out
    }

    fn feed(assembler: &mut Assembler, packets: &[Vec<u8>]) -> Vec<Frame> {
        packets.iter().filter_map(|packet| assembler.push(packet)).collect()
    }

    #[test]
    fn joins_blocks_in_order_and_strips_the_header_and_padding() {
        let mut assembler = Assembler::new();
        let bitstream: Vec<u8> = (0..70).collect();
        let mut all = packets(1, true, &bitstream, 2);
        // Packets inside a block may arrive in any order.
        all.swap(0, 2);
        let frames = feed(&mut assembler, &all);
        assert_eq!(frames, [Frame { data: bitstream, key: true, index: 1, timestamp: 1500, host_latency: 0x1234 }]);
        let frames = feed(&mut assembler, &packets(2, false, &[9; 8], 1));
        assert!(matches!(&frames[..], [Frame { index: 2, key: false, data, .. }] if data == &[9; 8]));
    }

    #[test]
    fn waits_for_a_key_frame_at_the_start() {
        let mut assembler = Assembler::new();
        assert!(assembler.needs_key_frame());
        assert!(feed(&mut assembler, &packets(1, false, &[1; 20], 1)).is_empty());
        assert!(assembler.needs_key_frame());
        assert!(matches!(&feed(&mut assembler, &packets(2, true, &[2; 20], 1))[..], [Frame { index: 2, .. }]));
        assert!(!assembler.needs_key_frame());
    }

    #[test]
    fn rebuilds_a_lost_packet_from_parity() {
        let bitstream: Vec<u8> = (0..150).map(|byte| byte as u8 ^ 0x5a).collect();
        for lost in 0..10 {
            // Ten data packets and two parity packets; one data packet and
            // the first parity packet are lost.
            let mut all = packets(1, true, &bitstream, 1);
            assert_eq!(all.len(), 12);
            all.remove(10);
            all.remove(lost);
            let frames = feed(&mut Assembler::new(), &all);
            assert!(matches!(&frames[..], [Frame { data, .. }] if data == &bitstream), "packet {lost}");
        }
    }

    #[test]
    fn a_damaged_packet_does_not_pass_as_a_rebuilt_frame() {
        let mut all = packets(1, true, &[7; 150], 1);
        all.remove(3);
        // A wrong byte in a received packet makes the rebuilt flags implausible.
        all[0][24] ^= 0x40;
        let mut assembler = Assembler::new();
        assert!(feed(&mut assembler, &all).is_empty());
        assert!(assembler.needs_key_frame());
    }

    #[test]
    fn a_frame_header_that_claims_too_little_data_is_dropped() {
        // One packet whose header says that one byte of it is used: less than
        // the frame header itself.
        let mut all = packets(1, true, &[], 1);
        all[0][HEADER + 4..HEADER + 6].copy_from_slice(&1u16.to_le_bytes());
        let mut assembler = Assembler::new();
        assert!(feed(&mut assembler, &all[..1]).is_empty());
        assert!(assembler.needs_key_frame());
    }

    #[test]
    fn more_loss_than_parity_needs_a_key_frame_until_one_arrives_whole() {
        let mut assembler = Assembler::new();
        feed(&mut assembler, &packets(1, true, &[1; 40], 1));
        // Three data packets and one parity packet; two data packets are lost.
        let mut broken = packets(2, false, &[2; 40], 1);
        broken.drain(1..3);
        assert!(feed(&mut assembler, &broken).is_empty());
        // The next frame shows the loss, and is useless until a key frame.
        assert!(!assembler.needs_key_frame());
        assert!(feed(&mut assembler, &packets(3, false, &[3; 40], 1)).is_empty());
        assert!(assembler.needs_key_frame());
        // The key frame the host sends in answer is lost too: keep asking.
        let mut broken_key = packets(4, true, &[4; 40], 1);
        broken_key.drain(1..3);
        assert!(feed(&mut assembler, &broken_key).is_empty());
        assert!(feed(&mut assembler, &packets(5, false, &[5; 40], 1)).is_empty());
        assert!(assembler.needs_key_frame());
        assert!(matches!(&feed(&mut assembler, &packets(6, true, &[6; 40], 1))[..], [Frame { index: 6, key: true, .. }]));
        assert!(!assembler.needs_key_frame());
        // Late packets of an old frame change nothing.
        assert!(feed(&mut assembler, &packets(2, false, &[2; 40], 1)).is_empty());
        assert!(!assembler.needs_key_frame());
    }

    #[test]
    fn a_skipped_frame_or_block_needs_a_key_frame() {
        let mut assembler = Assembler::new();
        feed(&mut assembler, &packets(1, true, &[1; 40], 1));
        assert!(feed(&mut assembler, &packets(3, false, &[3; 40], 1)).is_empty());
        assert!(assembler.needs_key_frame());
        let mut assembler = Assembler::new();
        feed(&mut assembler, &packets(1, true, &[1; 40], 1));
        let two_blocks = packets(2, false, &[1; 100], 2);
        assert!(feed(&mut assembler, &two_blocks[two_blocks.len() / 2..]).is_empty());
        assert!(assembler.needs_key_frame());
    }

    #[test]
    fn a_frame_left_unfinished_when_the_host_goes_quiet_needs_a_key_frame() {
        let mut assembler = Assembler::new();
        feed(&mut assembler, &packets(1, true, &[1; 40], 1));
        // Quiet after a whole frame: nothing is lost.
        assembler.idle();
        assert!(!assembler.needs_key_frame());
        // The end of the last frame before a still picture never comes.
        let mut cut = packets(2, false, &[2; 40], 1);
        cut.truncate(1);
        assert!(feed(&mut assembler, &cut).is_empty());
        assert!(!assembler.needs_key_frame());
        assembler.idle();
        assert!(assembler.needs_key_frame());
        // Its missing packets, arriving after all, change nothing.
        assert!(feed(&mut assembler, &packets(2, false, &[2; 40], 1)).is_empty());
        assert!(matches!(&feed(&mut assembler, &packets(3, true, &[3; 40], 1))[..], [Frame { index: 3, key: true, .. }]));
    }
}
