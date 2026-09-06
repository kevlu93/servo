use log::error;
use num_traits::Zero;

use crate::{audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo}, block::{Chunk, FRAMES_PER_BLOCK_USIZE}, delay_node::{CachedUpmixedBlock, DelayBuffer}};


#[derive(AudioNodeCommon)]
pub(crate) struct DelayWriter {
    channel_info: ChannelInfo,
    // Ring buffer where we push to the front
    // Easier mental model since entries in the back are the oldest
    delay_line: DelayBuffer,
    // Block that has been upmixed based on the channel count of the output
    upmixed_block: CachedUpmixedBlock,
    // Passed from the DelayNode
    max_delay_time: f64,
}

impl DelayWriter {
    pub(super) fn new(buffer: DelayBuffer, upmixed_block: CachedUpmixedBlock, channel_info: ChannelInfo, max_delay_time: f64) -> Self {
        Self {
            channel_info,
            delay_line: buffer,
            upmixed_block: upmixed_block,
            max_delay_time,
        }
    }

    fn update_delay_line_capacity(&self, capacity: usize) {
        // Only update if the capacity is currently 0
        if let Ok(mut delay_line) = self.delay_line.write() {
            if delay_line.capacity().is_zero() {
                delay_line.reserve(capacity);
            }
        };
    }

    /// Writes the input block to the delay line
    fn write(&self, mut input: Chunk) {
        {
            // First remove the oldest values if we are at capacity (max_delay_time)
            let Some(block) = input.blocks.pop() else {
                error!(
                    "Attempted to write a chunk with no data to the delay node internal buffer. This should not have occurred due to size check prior to write"
                );
                return;
            };
            let Ok(mut delay_line) = self.delay_line.write() else {
                error!("Unable to acquire write lock for the delay buffer.");
                return;
            };
            let last_index = delay_line.capacity() - 1;
            delay_line.truncate(last_index);
            delay_line.push_front(block);
        }
        // Shift the index of the existing upmixed block
        let Ok(mut upmixed_block) = self.upmixed_block.write() else {
                error!("Unable to acquire write lock for the upmixed block.");
                return;
            };
        if let Some(upmixed_block) = &mut *upmixed_block {
            upmixed_block.increment_index();
        }
    }
}

impl AudioNodeEngine for DelayWriter {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::DelayWriter
    }

    fn process(&mut self, inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);

        let max_delay_frame = self.max_delay_time * info.sample_rate as f64;
        let max_delay_block = (max_delay_frame / FRAMES_PER_BLOCK_USIZE as f64).ceil() as usize;

        // Update the delay line capacity if necessary, and write the input blocks to the internal buffer
        self.update_delay_line_capacity(max_delay_block + 1);

        // Read from the internal buffer
        self.write(inputs);
        Chunk::default()
    }
}
