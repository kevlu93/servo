/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use log::error;
use malloc_size_of_derive::MallocSizeOf;
use num_complex::Complex64;
use realfft::RealFftPlanner;
use crate::{audio_node::{AudioNodeEngine, AudioNodeType, BlockInfo, ChannelInfo}, block::{Block, Chunk, FRAMES_PER_BLOCK_USIZE}, buffer_source_node::AudioBuffer};

#[derive(Clone, Debug, MallocSizeOf)]
pub struct ConvolverNodeOptions {
    pub buffer: Option<AudioBuffer>,
    pub normalize: bool,
}

#[derive(Clone, Debug, MallocSizeOf)]
pub enum ConvolverNodeMessage {
    SetBuffer(Option<AudioBuffer>),
    SetNormalize(bool),
}

#[derive(AudioNodeCommon)]
pub(crate) struct ConvolverNode {
    channel_info: ChannelInfo,
    buffer: Option<AudioBuffer>,
    normalize: bool,
    normalization_scale: Option<f64>,
    buffer_ffts: Option<Vec<Vec<Complex64>>>
}

fn calculate_normalization_scale(buffer: &Option<AudioBuffer>, normalize: bool) -> Option<f64> {
    if normalize {
        buffer.as_ref().map(|buffer|{

    let gain_calibration = 0.00125_f64;
    let gain_calibration_sample_rate = 44100_f64;
    let min_power = 0.000125_f64;
    // Normalize by RMS power.
    let number_of_channels = buffer.chans() as f64;
    let buffer_length = buffer.len() as f64;
    
    let mut power = buffer.buffers.iter().fold(0_f64, |power, channel| {
        power + channel.iter().fold(0_f64, |channel_power, sample| channel_power + sample.powi(2) as f64)
    });
    
    power = (power / (number_of_channels * buffer_length)).sqrt();
    if power.is_infinite() {
        power = min_power;
    }
    power = power.max(min_power);
    let mut scale = 1.0 / power;
    // Calibrate to make perceived volume same as unprocessed.
    scale *= gain_calibration;
    // Scale depends on sample-rate.
    scale *= gain_calibration_sample_rate / buffer.sample_rate as f64;
    // True-stereo compensation.
    if number_of_channels == 4.0 {
        scale *= 0.5;
    }
    scale
        })
    } else {
        None
    }
}

fn calculate_buffer_fft(buffer: &[f32]) -> Vec<Complex64> {
    let length = (FRAMES_PER_BLOCK_USIZE + (buffer.len() -1) * 2 - 1).div_ceil(FRAMES_PER_BLOCK_USIZE) * FRAMES_PER_BLOCK_USIZE;
    let mut planner = RealFftPlanner::<f64>::new();
    // Calculate FFTs for the input and impulse response
    let fft = planner.plan_fft_forward(length);
    let mut input = buffer.iter().map(|x| *x as f64).collect::<Vec<_>>();
    input.resize(length, 0.0);
    let mut output = fft.make_output_vec();
    fft.process(&mut input, &mut output).unwrap();
    output
}

// Quickly calculate the linear convolution of the input and buffer FFT
// Based on convolution theorem
fn linear_convolution(input: &[f32], impulse_response_fft: &[Complex64]) -> Vec<f64> {
    // We want the next multiple of FRAMES_PER_BLOCK >= N + M - 1
    // Remember that impulse response fft is size N/2 + 1
    let length = (impulse_response_fft.len() - 1) * 2;
    let mut planner = RealFftPlanner::<f64>::new();
    // Calculate FFTs for the input and impulse response
    let fft = planner.plan_fft_forward(length);
    let mut input = input.iter().map(|x| *x as f64).collect::<Vec<_>>();
    input.resize(length, 0.0);
    let mut output = fft.make_output_vec();
    fft.process(&mut input, &mut output).unwrap();

    // Calculate the element wise product
    let mut product = output.iter().zip(impulse_response_fft.iter()).map(|(f, g)| f * g).collect::<Vec<_>>();

    // Now take the inverse FFT of the product
    let ifft = planner.plan_fft_inverse(length);
    let mut convolution_output = ifft.make_output_vec();
    ifft.process(&mut product, &mut convolution_output).unwrap();
    convolution_output.iter().map(|x| *x / length as f64).collect::<Vec<_>>()
}

impl ConvolverNode {
    pub fn new(options: ConvolverNodeOptions, channel_info: ChannelInfo) -> Self {
        let normalization_scale = calculate_normalization_scale(&options.buffer, options.normalize);
        Self {
            channel_info,
            buffer: options.buffer,
            normalize: options.normalize,
            normalization_scale,
            buffer_ffts: Default::default()
        }
    }

    fn handle_convolver_message(&mut self, message: ConvolverNodeMessage, _sample_rate: f32) {
        match message {
            ConvolverNodeMessage::SetBuffer(buffer) => {
                self.buffer = buffer;
                self.normalization_scale = calculate_normalization_scale(&self.buffer, self.normalize);
                // Precompute buffer FFTs
                self.buffer_ffts = self.buffer.as_ref().map(|buffers| buffers.buffers.iter().map(|buffer| calculate_buffer_fft(buffer.as_slice())).collect());
            },
            ConvolverNodeMessage::SetNormalize(normalize) => {
                self.normalize = normalize;
            },
        }
    }
}

impl AudioNodeEngine for ConvolverNode {
    fn node_type(&self) -> AudioNodeType {
        AudioNodeType::ConvolverNode
    }

    fn process(&mut self, inputs: Chunk, _info: &BlockInfo) -> Chunk {
        debug_assert!(inputs.len() == 1);

        let Some(buffer) = self.buffer.as_ref() else {
            return inputs;
        };

        let Some(buffer_ffts) = self.buffer_ffts.as_ref() else {
            return inputs;
        };

        let input_block = &inputs.blocks[0];
        let mut convolution_output = match (input_block.chan_count(), buffer.chans()) {
            // Mono with Mono
            (1, 1) => {
                linear_convolution(input_block.data_chan(0), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>()
            }
            // Mono with Stereo Response
            (1, 2) => {
                let output_0 = linear_convolution(input_block.data_chan(0), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_1 = linear_convolution(input_block.data_chan(0), buffer_ffts[1].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();

                let mut output = Vec::with_capacity(output_0.len() * 2);
                output.extend(output_0);
                output.extend(output_1);
                output
            }
            // Stereo with Mono Response
            (2, 1) => {
                let output_0 = linear_convolution(input_block.data_chan(0), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_1 = linear_convolution(input_block.data_chan(1), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();

                let mut output = Vec::with_capacity(output_0.len() * 2);
                output.extend(output_0);
                output.extend(output_1);
                output
            }
            // Stereo with Stereo Response
            (2, 2) => {
                let output_0 = linear_convolution(input_block.data_chan(0), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_1 = linear_convolution(input_block.data_chan(1), buffer_ffts[1].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();

                let mut output = Vec::with_capacity(output_0.len() * 2);
                output.extend(output_0);
                output.extend(output_1);
                output
            }
            // Stereo with "true" Stereo Matrix Response
            (2, 4) => {
                let output_0 = linear_convolution(input_block.data_chan(0), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_1 = linear_convolution(input_block.data_chan(0), buffer_ffts[1].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_2 = linear_convolution(input_block.data_chan(1), buffer_ffts[2].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_3 = linear_convolution(input_block.data_chan(1), buffer_ffts[3].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let mut output = Vec::with_capacity(output_0.len() * 4);
                output.extend(output_0);
                output.extend(output_1);
                output.extend(output_2);
                output.extend(output_3);
                output
            }
            // Mono with Stereo Matrix Response
            (1, 4) => {
                let output_0 = linear_convolution(input_block.data_chan(0), buffer_ffts[0].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_1 = linear_convolution(input_block.data_chan(0), buffer_ffts[1].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_2 = linear_convolution(input_block.data_chan(0), buffer_ffts[2].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let output_3 = linear_convolution(input_block.data_chan(0), buffer_ffts[3].as_slice()).iter().map(|x| *x as f32).collect::<Vec<_>>();
                let mut output = Vec::with_capacity(output_0.len() * 4);
                output.extend(output_0);
                output.extend(output_1);
                output.extend(output_2);
                output.extend(output_3);
                output
            }
            _ => {
                error!("Invalid channel configuration. Input channels: {}, Impulse Response channels: {}", input_block.chan_count(), buffer.chans());
                return inputs;
            }
        };
        
        if self.normalize {
            // Take this calculated normalizationScale value and
            // multiply it by the result of the linear convolution resulting from processing 
            // the input with the impulse response (represented by the buffer) to produce the final output.
            if let Some(normalization_scale) = self.normalization_scale {
                convolution_output = convolution_output.into_iter().map(|x| (x as f64 * normalization_scale) as f32).collect();
            }
        }
        let block = Block::for_vec(convolution_output);
        let mut chunk = Chunk::default();
        chunk.blocks.push(block);
        chunk
    }
make_message_handler!(
        ConvolverNode: handle_convolver_message
    );
}
