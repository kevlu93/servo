/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};

use f32;
use malloc_size_of_derive::MallocSizeOf;

use crate::audio_node::{
    AudioNodeEngine, AudioNodeMessage, AudioNodeType, BlockInfo, ChannelInfo, ChannelInterpretation
};
use crate::block::{Block, Chunk};
use crate::param::{Param, ParamRate, ParamType, UserAutomationEvent};

mod delay_writer;
mod delay_reader;
pub(crate) use delay_reader::DelayReader;
pub(crate) use delay_writer::DelayWriter;

// Share with internal nodes. Use Arc because AudioNodeEngine requires Send
type DelayBuffer = Arc<RwLock<VecDeque<Block>>>;
type CachedUpmixedBlock = Arc<RwLock<Option<UpmixedBlock>>>;
type SharedParam = Arc<RwLock<Param>>;

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
    shared_delay_time: SharedParam,
    delay_writer: DelayWriter,
    delay_reader: DelayReader,
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

    fn increment_index(&mut self) {
        self.index += 1;
    }
}

impl DelayNode {
    pub fn new(options: DelayNodeOptions, channel_info: ChannelInfo) -> Self {
        let delay_line = Arc::new(RwLock::new(VecDeque::with_capacity(0)));
        let upmixed_block = Arc::new(RwLock::new(None));
        let delay_time = Param::new(options.delay_time as f32);
        let shared_delay_time = Arc::new(RwLock::new(delay_time.clone()));
        DelayNode {
            channel_info: channel_info,
            delay_time,
            shared_delay_time: shared_delay_time.clone(),
            delay_writer: DelayWriter::new(delay_line.clone(), upmixed_block.clone(), channel_info, options.max_delay_time),
            delay_reader: DelayReader::new(delay_line.clone(), upmixed_block.clone(), shared_delay_time.clone(), channel_info),
        }
    }

    fn set_param(&mut self, id: ParamType, event: UserAutomationEvent, sample_rate: f32) {
        match id {
            ParamType::DelayTime => {
                let mut delay_time = self.shared_delay_time.write().unwrap();
                delay_time.insert_event(event.convert_to_event(sample_rate))

            },
            _ => panic!("Unknown param {:?} for DelayNode", id),
        }
    }

    fn set_param_rate(&mut self, id: ParamType, rate: ParamRate) {
        match id {
            ParamType::DelayTime => {
                let mut delay_time = self.shared_delay_time.write().unwrap();
                delay_time.set_rate(rate)

            },
            _ => panic!("Unknown param {:?} for DelayNode", id),
        }
    }
}

impl AudioNodeEngine for DelayNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::DelayNode
    }

    fn process(&mut self, inputs: Chunk, info: &BlockInfo) -> Chunk {
        self.delay_writer.process(inputs, info);

        // Read from the internal buffer
        self.delay_reader.process(Chunk::default(), info)
    }

    fn message(&mut self, msg: AudioNodeMessage, sample_rate: f32) {
        match msg {
            AudioNodeMessage::GetParamValue(id, tx) => {
                let _ = tx.send(self.get_param(id).value());
            },
            AudioNodeMessage::SetChannelCount(c) => self.set_channel_count(c),
            AudioNodeMessage::SetChannelMode(c) => self.set_channel_count_mode(c),
            AudioNodeMessage::SetChannelInterpretation(c) => self.set_channel_interpretation(c),
            // SetParam and SetParamRate behave differently because delay time must be shared with DelayReader.
            // However, DelayReader is internal and the Param can only be set through the DelayNode.
            AudioNodeMessage::SetParam(id, event) => self.set_param(id, event, sample_rate),
            AudioNodeMessage::SetParamRate(id, rate) => self.set_param_rate(id, rate),
            _ => self.message_specific(msg, sample_rate),
        }
    }

    fn get_param(&mut self, id: ParamType) -> &mut Param {
        let delay_time = self.shared_delay_time.write().unwrap();
        self.delay_time = delay_time.clone();
        match id {
            ParamType::DelayTime => &mut self.delay_time,
            _ => panic!("Unknown param {:?} for DelayNode", id),
        }
    }
}
