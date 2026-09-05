/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;
use std::collections::VecDeque;

use f32;
use malloc_size_of_derive::MallocSizeOf;
use num_traits::{Euclid, Zero};
use smallvec::smallvec;

use crate::audio_node::{
    AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo, ChannelInterpretation,
};
use crate::block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE, Tick};
use crate::param::{Param, ParamType};

#[derive(Copy, Clone, Debug, MallocSizeOf)]
pub struct DelayNodeOptions {
    pub max_delay_time: f64,
    pub delay_time: f64,
}

impl Default for DelayNodeOptions {
    fn default() -> Self {
        DelayNodeOptions {
            max_delay_time: 1.,
            delay_time: 0.,
        }
    }
}

#[derive(AudioNodeCommon)]
pub(crate) struct DelayNode {
    channel_info: ChannelInfo,
    delay_time: Param,
    max_delay_time: f64,
    // Tracks the delay time in terms of number of frames for each frame in the input block
    // When reading from the buffer, we look for the stored block with the relevant frames
    delay_frames: Vec<f32>,
    /// Ring buffer where we push to the front
    /// Easier mental model since entries in the back are the oldest
    delay_buffer: VecDeque<Block>,
    /// Block that has been upmixed based on the channel count of the output
    upmixed_block: Option<UpmixedBlock>,
}

#[derive(Debug)]
struct UpmixedBlock {
    index: usize,
    block: Block,
}

impl UpmixedBlock {
    fn new(
        index: usize,
        channel_count: u8,
        channel_interpretation: ChannelInterpretation,
        block: &Block,
    ) -> Self {
        let mut block = block.clone();
        block.mix(channel_count, channel_interpretation);
        UpmixedBlock { index, block }
    }

    fn get_index(&self) -> usize {
        self.index
    }

    fn get_block(&self) -> &Block {
        &self.block
    }

    fn increment_index(&mut self)  {
        self.index += 1;
    }
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

fn calc_delay_frames(delay_time: f64, sample_rate: f64) -> f64 {
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
            delay_frames: Vec::with_capacity(FRAMES_PER_BLOCK_USIZE),
            delay_buffer: VecDeque::with_capacity(0),
            upmixed_block: Default::default(),
        }
    }

    pub fn update_parameters(&mut self, info: &BlockInfo, tick: Tick) -> bool {
        let updated = self.delay_time.update(info, tick);
        self.delay_frames
            .push(self.delay_time.value() * info.sample_rate);
        updated
    }

    fn find_block_with_index(&self, delay_frame_index: usize) -> usize {
        delay_frame_index / FRAMES_PER_BLOCK_USIZE
    }

    fn upmix_block(&mut self, index: usize, channel_count: u8, block: &Block) {
        if let Some(upmixed_block) = self.upmixed_block.as_ref() {
            if index == upmixed_block.get_index() {
                return;
            } 
        }
            self.upmixed_block = Some(UpmixedBlock::new(
                index,
                channel_count,
                self.channel_info.interpretation,
                block,
            ));
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

        let max_delay_frame = self.max_delay_time * info.sample_rate as f64;
        let max_delay_block = (max_delay_frame / FRAMES_PER_BLOCK_USIZE as f64).ceil() as usize;

        if self.delay_buffer.capacity().is_zero() {
            self.delay_buffer = VecDeque::with_capacity(max_delay_block + 1);
        }

        // Write a block to the internal buffer.
        // First remove the oldest values if we are at capacity (max_delay_time)
        self.delay_buffer.truncate(self.delay_buffer.capacity() - 1);
        self.delay_buffer.push_front(inputs.blocks[0].clone()); 
        // TODO: Should be able to shift the index of upmixed block
        self.upmixed_block = None;

        let mut iter = inputs.blocks[0].iter();
        let mut is_active = false;
        // First update the delay time parameters, getting us the delay frames to read from
        // We also calculate the range of delay frames.
        let mut delay_frames = Vec::with_capacity(FRAMES_PER_BLOCK_USIZE);
        while let Some(frame) = iter.next() {
            // Delay time can be a-rate, so we need to check each sample
            self.update_parameters(info, frame.tick());
            let delay_frame = self.delay_time.value() * info.sample_rate;
            delay_frames.push(delay_frame);
        }
        let (min_delay_frame, max_delay_frame) = delay_frames
            .iter()
            .fold((f32::MAX, f32::MIN), |frames, delay_frame| {
                (frames.0.min(*delay_frame), frames.1.max(*delay_frame))
            });
        // With the range of delay frames we can check which blocks we will be reading from.
        let earlier_block = self.find_block_with_index(max_delay_frame.ceil() as usize);
        let later_block = self.find_block_with_index(min_delay_frame.floor() as usize);
        // Now search through the potential blocks for their channel counts
        // By construction earlier blocks are in higher indices of the delay line
        let mut channel_count = 0;
        for block in later_block..=(later_block.max(earlier_block.min(self.delay_buffer.len() - 1)))
        {
            channel_count = channel_count.max(
                self.delay_buffer
                    .get(block)
                    .map(|block| block.chan_count())
                    .unwrap_or_default(),
            );
        }
        // When there is a decrease in input channel count,
        // the change in output channel count MUST happen when the input that was received
        // with greater channel count no longer affects the output.
        // Therefore, we know that output channel count will be the highest channel count of the
        // blocks read.
        //
        // When there is an increase in input channel count, the behavior depends on the AudioNode type:
        // For a DelayNode or a DynamicsCompressorNode, the number of output channels MUST increase
        // when the input that was received with greater channel count begins to affect the output.
        // Thus, we start with an output channel count of 1, and increase if necessary.
        // https://webaudio.github.io/web-audio-api/#tail-time

        // If channel count is 0, then no data is outputted.
        // In this case we just return an output block with a single channel of 0s.
        if channel_count.is_zero() {
            return Chunk {
                blocks: smallvec![Block::for_channels_explicit(1)],
            };
        }

        // Initialize Vec to store output data
        let mut block_data: Vec<Vec<f32>> = Vec::with_capacity(channel_count as usize);
        for _ in 0..channel_count {
            let channel_data: Vec<f32> = Vec::with_capacity(FRAMES_PER_BLOCK_USIZE);
            block_data.push(channel_data);
        }

        let mut iter = inputs.blocks[0].iter();
        // First calculate delay frames to read from for each input frame
        while let Some(frame) = iter.next() {
            let mut delay_frame = delay_frames[frame.tick().0 as usize];
            //println!("delay_frame: {:?}", delay_frame);
            // Read a frame from internal buffer at the values indexed around delay_frames.
            // Write to buffer occurs first, so if delay_frame = 0 is the latest frame,
            // input block t, would be at delay frame (127 - t).
            // Therefore delay(t) must be increased by FRAMES_PER_BLOCK - 1 - t
            delay_frame += (FRAMES_PER_BLOCK_USIZE - 1 - frame.tick().0 as usize) as f32;
            //println!("delay_frame after adjusting: {:?}, tick: {:?}", delay_frame, frame.tick().0);
            let lower_frame_index = delay_frame.floor() as usize;
            let higher_frame_index = delay_frame.ceil() as usize;
            let lower_block_index = self.find_block_with_index(lower_frame_index);
            let higher_block_index = self.find_block_with_index(higher_frame_index);
            //println!("searching for blocks: {} {}", lower_block_index, higher_block_index);
            let mut linear_interpolation_factor = delay_frame.fract();
            let mut values = vec![0.; channel_count as usize];
            for (frame_index, block_index) in [
                (lower_frame_index, lower_block_index),
                (higher_frame_index, higher_block_index),
            ]
            .into_iter()
            {
                if !linear_interpolation_factor.is_zero() {
                    let Some(block) = self.delay_buffer.get(block_index) else {
                        continue;
                    };
                    //TODO: clean this up!!
                    let block = (*block).clone();
                    for channel in 0..channel_count as usize {
                        let position_for_block = frame_index % FRAMES_PER_BLOCK_USIZE;
                        //println!("position_for_block: {:?}", position_for_block);
                        // Remember that block buffer data goes from oldest to newest
                        self.upmix_block(block_index, channel_count, &block);
                            let upmixed_value = self.upmixed_block
                                .as_ref()
                                .map(|upmixed_block| {
                                    upmixed_block
                                        .get_block()
                                        .data_chan_frame(127 - position_for_block, channel as u8)
                                })
                                .unwrap_or_default();
                        values[channel] += linear_interpolation_factor * upmixed_value;

                    }
                }
                linear_interpolation_factor = 1. - linear_interpolation_factor;
            }
            //println!("values: {:?}\nblock_data: {:?}", values, block_data);
            values
                .into_iter()
                .zip(block_data.iter_mut())
                .for_each(|(value, channel)| channel.push(value));
            // Flag that the node is actively processing
            //if value >= f32::MIN && !is_active {
            //    is_active = true;
            //}
        }
        let mut check = 0.;

                            //println!("Failed to get data\nchannel_count: {}\nframe index:{}\noriginal block: {:?}\nupmixed block: {:?}", channel_count, delay_frames[0], self.delay_buffer.get(self.find_block_with_index(delay_frames[0].ceil() as usize)), self.upmixed_block);
        //self.upmixed_block = None;
        let mut block = Block::empty();
        //println!("output block data: {:?}", block_data);
        block_data
            .into_iter()
            .for_each(|channel| block.push_chan(channel.as_slice()));
        Chunk {
            blocks: smallvec![block; 1],
        }
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        match id {
            ParamType::DelayTime => &mut self.delay_time,
            _ => panic!("Unknown param {:?} for DelayNode", id),
        }
    }
}
