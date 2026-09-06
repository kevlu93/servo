use crate::{audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo}, block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE, Tick}, delay_node::{CachedUpmixedBlock, DelayBuffer, SharedParam, UpmixedBlock}};
use num_traits::Zero;

#[derive(AudioNodeCommon)]
pub(crate) struct DelayReader {
    channel_info: ChannelInfo,
    // Tracks the delay time in terms of number of frames for each frame in the input block
    // When reading from the buffer, we look for the stored block with the relevant frames
    delay_frames: [f32; FRAMES_PER_BLOCK_USIZE],
    // Ring buffer where we push to the front
    // Easier mental model since entries in the back are the oldest
    delay_line: DelayBuffer,
    // Block that has been upmixed based on the channel count of the output
    upmixed_block: CachedUpmixedBlock,
    // Shared delay_time param from the delay node
    delay_time: SharedParam,
}

fn find_block_with_index(delay_frame_index: usize) -> usize {
        delay_frame_index / FRAMES_PER_BLOCK_USIZE
    }

impl DelayReader {
    pub(super) fn new(buffer: DelayBuffer, upmixed_block: CachedUpmixedBlock, delay_time: SharedParam, channel_info: ChannelInfo) -> Self {
        DelayReader {
            channel_info,
            delay_frames: [0.; FRAMES_PER_BLOCK_USIZE],
            delay_line: buffer,
            upmixed_block: upmixed_block,
            delay_time,
        }
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-delaynode-delaytime>
    /// If DelayNode is part of a cycle, then the value of the delayTime attribute is clamped
    /// to a minimum of one render quantum.
    fn update_parameters(&mut self, info: &BlockInfo, tick: Tick, in_cycle: bool) -> bool {
        let updated = 
            self.delay_time.write().and_then(|mut delay_time| {
                Ok(delay_time.update(info, tick))
            }).unwrap_or_default();
        let delay_time = {
            let Ok(delay_time) = self.delay_time.read() else {
                return updated;
            };
            // TODO: Param needs to handle min and max value correctly, so that it updates with the
            // value clamped at minimum instead of clamping here
            if in_cycle {
                delay_time
                    .value()
                    .min(FRAMES_PER_BLOCK_USIZE as f32 / info.sample_rate)
            } else {
                delay_time.value()
            }};
        self.update_delay_frames(tick.0 as usize, delay_time * info.sample_rate);
        updated
    }

    fn update_delay_frames(&mut self, tick: usize, value: f32) {
        self.delay_frames[tick] = value;
    } 

    /// Calculates the output channel count
    /// <https://webaudio.github.io/web-audio-api/#tail-time>
    /// 4.3
    /// > When an AudioNode has a non-zero tail-time,
    /// > and an output channel count that depends on the input channels count,
    /// > the AudioNode’s tail-time must be taken into account when the input channel count changes.
    /// >
    /// > When there is a decrease in input channel count,
    /// > the change in output channel count MUST happen when the input that was received
    /// > with greater channel count no longer affects the output.
    /// >
    /// > When there is an increase in input channel count, the behavior depends on the AudioNode type:
    /// > * For a DelayNode or a DynamicsCompressorNode, the number of output channels MUST increase
    /// >   when the input that was received with greater channel count begins to affect the output.
    /// Therefore, we know that output channel count will be the highest channel count of the
    /// blocks read.
    fn calc_output_channel_count(&self) -> u8 {
        let (min_delay_frame, max_delay_frame) = self
            .delay_frames
            .iter()
            .fold((f32::MAX, f32::MIN), |frames, delay_frame| {
                (frames.0.min(*delay_frame), frames.1.max(*delay_frame))
            });
        // With the range of delay frames we can check which blocks we will be reading from.
        let earlier_block = find_block_with_index(max_delay_frame.ceil() as usize);
        let later_block = find_block_with_index(min_delay_frame.floor() as usize);
        // Now search through the potential blocks for their channel counts
        // By construction earlier blocks are in higher indices of the delay line
        let mut channel_count = 0;
        if let Ok(delay_line) = self.delay_line.read() {
        for block in later_block..=(later_block.max(earlier_block.min(delay_line.len() - 1))) {
            channel_count = channel_count.max(
                delay_line
                    .get(block)
                    .map(|block| {
                        // Silent blocks don't affect the output.
                        if !block.is_silence() {
                            block.chan_count()
                        } else {
                            0
                        }
                    })
                    .unwrap_or_default(),
            );
        }
        }
        channel_count
    }

    fn upmix_block(&self, index: usize, channel_count: u8, block: &Block) -> UpmixedBlock {
        UpmixedBlock::new(
                        index,
                        channel_count,
                        self.channel_info.interpretation,
                        block,
                    )
    }

    /// Read frames from the delay line at the values indexed around the specified delays.
    /// Write to the delay line occurs first, so if delay_frame = 0 is the latest frame written in,
    /// then input block t would be at delay frame (127 - t).
    /// When calculated, each delay(t) was calculated relative to t.
    /// Therefore when reading from the delay line,
    /// we are looking for delay(t) + FRAMES_PER_BLOCK - 1 - t
    pub(crate) fn read(&mut self, in_cycle: bool) -> Chunk {
        let channel_count = self.calc_output_channel_count();
        // If channel count is 0, then no data is outputted.
        // In this case we just return an output block with a single channel of 0s.
        if channel_count.is_zero() {
            return Chunk::explicit_silence();
        }
        let mut has_active_value = false;
        // Initialize the output block
        let mut output_block = Block::for_channels_explicit(channel_count);
        for (tick, delay_frame) in self.delay_frames.into_iter().enumerate() {
            let delay_frame = delay_frame + (FRAMES_PER_BLOCK_USIZE - 1 - tick) as f32;
            //println!("delay_frame after adjusting: {:?}, tick: {:?}", delay_frame, frame.tick().0);
            let lower_frame_index = delay_frame.floor() as usize;
            let higher_frame_index = delay_frame.ceil() as usize;
            let lower_block_index = find_block_with_index(lower_frame_index);
            let higher_block_index = find_block_with_index(higher_frame_index);
            //println!("searching for blocks: {} {}", lower_block_index, higher_block_index);
            let mut linear_interpolation_factor = delay_frame.fract();
            for (frame_index, block_index) in [
                (lower_frame_index, lower_block_index),
                (higher_frame_index, higher_block_index),
            ]
            .into_iter()
            {
                if !linear_interpolation_factor.is_zero() {
                    let Ok(delay_line) = self.delay_line.read() else {
                        continue;
                    };
                    {
                        let Some(block) = delay_line.get(block_index) else {
                            continue;
                        };
                        let Ok(mut maybe_upmixed_block) = self.upmixed_block.write() else {
                            continue;
                        };
                        if let Some(upmixed_block) = maybe_upmixed_block.as_ref() {
                            if upmixed_block.get_index() != block_index {
                                *maybe_upmixed_block = Some(self.upmix_block(block_index, channel_count, block));
                            }
                        } else {
                            *maybe_upmixed_block = Some(self.upmix_block(block_index, channel_count, &block));
                        }
                    }
                    //TODO: clean this up!!
                    for channel in 0..channel_count as usize {
                        let position_for_block = frame_index % FRAMES_PER_BLOCK_USIZE;
                        //println!("position_for_block: {:?}", position_for_block);
                        // Remember that block buffer data goes from oldest to newest
                        let upmixed_value = {
                            let Ok(upmixed_block) = self.upmixed_block.read() else {
                                continue;
                            };
                            upmixed_block
                            .as_ref()
                            .map(|upmixed_block| {
                                upmixed_block
                                    .get_block()
                                    .data_chan_frame(127 - position_for_block, channel as u8)
                            })
                            .unwrap_or_default()};
                        if upmixed_value.abs() >= f32::MIN && !has_active_value {
                            has_active_value = true;
                        }
                        let output_channel = output_block.data_chan_mut(channel as u8);
                        output_channel[tick] += linear_interpolation_factor * upmixed_value;
                    }
                }
                linear_interpolation_factor = 1. - linear_interpolation_factor;
            }
        }
        // 1.5.3
        // > A DelayNode in a cycle is actively processing only when
        // the absolute value of any output sample for the current render quantum
        // is greater than or equal to 2^−126.
        //
        // > AudioNodes that are not actively processing output a single channel of silence.
        if in_cycle && !has_active_value {
            Chunk::explicit_silence()
        } else {
            Chunk::new(output_block)
        }
    }
}

impl AudioNodeEngine for DelayReader {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::DelayNode
    }

    fn process(&mut self, _inputs: Chunk, info: &BlockInfo) -> Chunk {

        // TODO: Need to implement delay node cycle breaking
        let in_cycle = false;

        // Reset the delay frames array
        self.delay_frames = [0.; FRAMES_PER_BLOCK_USIZE];
        
        for i in 0..FRAMES_PER_BLOCK_USIZE {
            self.update_parameters(info, Tick(i as u64), in_cycle);
        }

        // Read from the internal buffer
        self.read(in_cycle)
    }
}
