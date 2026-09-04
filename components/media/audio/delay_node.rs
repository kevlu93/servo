/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::VecDeque;

use malloc_size_of_derive::MallocSizeOf;
use smallvec::smallvec;

use crate::audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE, Tick};
use crate::param::{Param, ParamType};
use num_traits::Zero;

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub struct DelayNodeOptions {
    pub max_delay_time: f64,
    pub delay_time: f64,
}

impl Default for DelayNodeOptions {
    fn default() -> Self {
        DelayNodeOptions { max_delay_time: 1., delay_time: 0.}
    }
}

#[derive(AudioNodeCommon)]
pub(crate) struct DelayNode {
    channel_info: ChannelInfo,
    delay_time: Param,
    max_delay_time: f64,
    // Delay time in terms of number of frames
    // This functions as the pointer to the frame written delayTime ago
    delay_frames: f32,
    /// Ring buffer where we push to the front and read from the frame at delay_frames
    /// Easier mental model since entries in the back are the oldest
    /// If write will result in more entries than the capacity,
    /// the oldest samples will be discarded
    delay_buffer: VecDeque<f32>,
}

struct DelayBuffer {
    // Delay time in terms of number of frames
    delay_frames: f32,
    // Delay time param from the delay node
    delay_time: RefCell<Param>,
    // A ring buffer of size floor(delay_frames)
    // Use floor because when we hit the limit,
    // we calculate linear interpolation of the frames surrounding the limit (if not an integer)
    block_buffers: VecDeque<f32>,
    // Leftover frames from process run are held here,
    // to be drained in the next run
    leftover_frames: VecDeque<f32>,
}

fn calc_delay_frames(delay_time: f64, sample_rate: f64) -> f64{
    delay_time * sample_rate * FRAMES_PER_BLOCK_USIZE as f64
}

impl DelayBuffer {
    fn new(options: DelayNodeOptions, delay_time: RefCell<Param>) -> Self {
        Self {
            delay_frames: -1.,
            delay_time,
            block_buffers: VecDeque::new(),
            // Overflow frames are also set at max_delay_frames.
            // This is to handle the case where delay time is set at max,
            // and then gets set to a much smaller value.
            leftover_frames: VecDeque::new(),
        }
    }
}

impl DelayNode {
    pub fn new(options: DelayNodeOptions, channel_info: ChannelInfo) -> Self {
        Self {
            channel_info,
            delay_time: Param::new(options.delay_time as f32),
            max_delay_time: options.max_delay_time,
            // Can't calculate until we have sample rate from the block info
            delay_frames: -1.,
            delay_buffer: VecDeque::with_capacity(0),
        }
    } 

    pub fn update_parameters(&mut self, info: &BlockInfo, tick: Tick) -> bool {
        let updated = self.delay_time.update(info, tick);
        if updated || self.delay_frames < 0. {
            self.delay_frames = self.delay_time.value() * info.sample_rate;
            let max_delay_frames = self.max_delay_time * info.sample_rate as f64;
            println!("delay_frames: {:?}, max_delay_frames: {:?}", self.delay_frames, max_delay_frames);
            let max_index = max_delay_frames.ceil() as usize;
            if self.delay_buffer.capacity() != max_index {
                if max_index >= self.delay_buffer.capacity() {
                    self.delay_buffer.reserve(max_index - self.delay_buffer.capacity());
                } else {
                    self.delay_buffer.truncate(max_index - 1);
                    self.delay_buffer.shrink_to(max_index);
                }
            }
        }
        updated
    }
}

impl AudioNodeEngine for DelayNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::DelayNode
    }

    fn process(&mut self, mut inputs: Chunk, info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);

        if inputs.blocks[0].is_silence() {
            todo!();
        }

        let mut iter = inputs.blocks[0].iter();
        let mut block_data = Vec::with_capacity(FRAMES_PER_BLOCK_USIZE);
        while let Some(frame) = iter.next() {
            // Delay time can be a-rate, so we need to check each sample
            self.update_parameters(info, frame.tick());
            // Read a frame from internal buffer at the values indexed around delay_frames. 
            // Delay buffer is such that at time t, index 0 has the frame from t - 1
            let index = self.delay_frames.floor() as usize;
            let linear_interpolation_factor = self.delay_frames.fract();
            let later_value = if index == 0 {
                // For a delay < 1 frame, the later frame needs to be the current frame
                frame.get_frame()[0]
            } else {
                self.delay_buffer
                    .get(index - 1)
                    .copied()
                    .unwrap_or_default()
            };

            let earlier_value = 
                self.delay_buffer
                    .get(index)
                    .copied()
                    .unwrap_or_default();

            let value = (1. - linear_interpolation_factor) * later_value + linear_interpolation_factor * earlier_value;
            block_data.push(value);

            // Write a frame to the internal buffer.
            // First remove the oldest values if we are at capacity (max_delay_time)
            self.delay_buffer.truncate(self.delay_buffer.capacity() - 1);
            let input = frame.get_frame();
            self.delay_buffer.push_front(input[0]);
        }
        let block = Block::for_vec(block_data);
        Chunk { blocks: smallvec![block; 1] }
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::DelayTime => &mut self.delay_time,
            _ => panic!("Unknown param {:?} for DelayNode", id),
        }
    }
}
