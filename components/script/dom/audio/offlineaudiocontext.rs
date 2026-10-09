/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, mpsc};
use std::thread::Builder;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_base::id::PipelineId;
use servo_media::audio::block::{FRAMES_PER_BLOCK, FRAMES_PER_BLOCK_USIZE};
use servo_media::audio::context::OfflineAudioContextOptions as ServoMediaOfflineAudioContextOptions;

use crate::dom::audio::audiobuffer::{AudioBuffer, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};
use crate::dom::audio::audionode::MAX_CHANNEL_COUNT;
use crate::dom::audio::baseaudiocontext::{BaseAudioContext, BaseAudioContextOptions};
use crate::dom::audio::offlineaudiocompletionevent::OfflineAudioCompletionEvent;
use crate::dom::bindings::codegen::Bindings::BaseAudioContextBinding::BaseAudioContext_Binding::BaseAudioContextMethods;
use crate::dom::bindings::codegen::Bindings::OfflineAudioContextBinding::{
    OfflineAudioContextMethods, OfflineAudioContextOptions,
};
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct OfflineAudioContext {
    context: BaseAudioContext,
    channel_count: u32,
    length: u32,
    rendering_started: Cell<bool>,
    pending_rendering_promise: DomRefCell<Option<TracedPromise>>,
    pending_suspend_promises: DomRefCell<BTreeMap<u64, TracedPromise>>,
}

impl OfflineAudioContext {
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new_inherited(
        channel_count: u32,
        length: u32,
        sample_rate: f32,
        pipeline_id: PipelineId,
    ) -> Fallible<OfflineAudioContext> {
        let options = ServoMediaOfflineAudioContextOptions {
            channels: channel_count as u8,
            length: length as usize,
            sample_rate,
        };
        let context = BaseAudioContext::new_inherited(
            BaseAudioContextOptions::OfflineAudioContext(options),
            pipeline_id,
        )?;
        Ok(OfflineAudioContext {
            context,
            channel_count,
            length,
            rendering_started: Cell::new(false),
            pending_rendering_promise: Default::default(),
            pending_suspend_promises: Default::default(),
        })
    }

    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    fn new(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        channel_count: u32,
        length: u32,
        sample_rate: f32,
    ) -> Fallible<DomRoot<OfflineAudioContext>> {
        if channel_count > MAX_CHANNEL_COUNT ||
            channel_count == 0 ||
            length == 0 ||
            !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate)
        {
            return Err(Error::NotSupported(None));
        }
        let pipeline_id = window.pipeline_id();
        let context =
            OfflineAudioContext::new_inherited(channel_count, length, sample_rate, pipeline_id)?;
        Ok(reflect_dom_object_with_proto(
            cx,
            Box::new(context),
            window,
            proto,
        ))
    }
}

impl OfflineAudioContextMethods<crate::DomTypeHolder> for OfflineAudioContext {
    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-offlineaudiocontext>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        options: &OfflineAudioContextOptions,
    ) -> Fallible<DomRoot<OfflineAudioContext>> {
        OfflineAudioContext::new(
            cx,
            window,
            proto,
            options.numberOfChannels,
            options.length,
            *options.sampleRate,
        )
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-offlineaudiocontext-numberofchannels-length-samplerate>
    fn Constructor_(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        number_of_channels: u32,
        length: u32,
        sample_rate: Finite<f32>,
    ) -> Fallible<DomRoot<OfflineAudioContext>> {
        OfflineAudioContext::new(cx, window, proto, number_of_channels, length, *sample_rate)
    }

    // https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-oncomplete
    event_handler!(complete, GetOncomplete, SetOncomplete);

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-length>
    fn Length(&self) -> u32 {
        self.length
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-startrendering>
    fn StartRendering(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if self.rendering_started.get() {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }
        self.rendering_started.set(true);

        *self.pending_rendering_promise.safe_borrow_mut(cx.no_gc()) = Some(promise.to_traced());

        let length = self.length as usize;
        let channel_count = self.channel_count as usize;
        let (sender, receiver) = mpsc::channel();
        let sender = Mutex::new(sender);
        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .set_eos_callback(Box::new(move |buffer| {
                let channels: Vec<Vec<f32>> = (*buffer)
                    .as_ref()
                    .chunks(length)
                    .take(channel_count)
                    .map(|channel| channel.to_vec())
                    .collect();
                let _ = sender.lock().unwrap().send(channels);
            }));

        let this = Trusted::new(self);
        let task_source = self
            .global()
            .task_manager()
            .dom_manipulation_task_source()
            .to_sendable();
        Builder::new()
            .name("OfflineACResolver".to_owned())
            .spawn(move || {
                let processed_audio = receiver.recv().unwrap_or_default();
                task_source.queue(task!(resolve: move |cx| {
                    let this = this.root();
                    let mut processed_audio = processed_audio;
                    // it can end up being empty if the task failed
                    processed_audio.resize_with(this.channel_count as usize, || {
                        vec![0.; this.length as usize]
                    });
                    let buffer = AudioBuffer::new_with_channel_data(
                        cx,
                        this.global().as_window(),
                        this.length,
                        *this.context.SampleRate(),
                        processed_audio,
                    );
                    let promise = {
                        (*this
                            .pending_rendering_promise
                            .safe_borrow_mut(cx.no_gc()))
                            .take()
                            .unwrap()
                            .root(cx)
                    };
                    promise.resolve_native(cx, &buffer);
                    let global = &this.global();
                    let window = global.as_window();
                    let event = OfflineAudioCompletionEvent::new(cx, window,
                                                                 atom!("complete"),
                                                                 EventBubbles::DoesNotBubble,
                                                                 EventCancelable::NotCancelable,
                                                                 &buffer);
                    event.upcast::<Event>().fire(cx, this.upcast());
                }));
            })
            .unwrap();

        if self
            .context
            .audio_context_impl()
            .lock()
            .unwrap()
            .resume()
            .is_none()
        {
            promise.reject_error(
                cx,
                Error::Type(c"Could not start offline rendering".to_owned()),
            );
        }

        promise
    }

    /// <https://webaudio.github.io/web-audio-api/#dom-offlineaudiocontext-suspend>
    fn Suspend(&self, cx: &mut CurrentRealm, suspend_time: Finite<f64>) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        // > [T]he specified time, which is quantized and rounded up to the render quantum size.
        // > If the quantized frame number
        // > 1. is negative or
        // > 2. is less than or equal to the current time or
        // > 3. is greater than or equal to the total render duration or
        // > 4.is scheduled by another suspend for the same time,
        // > then the promise is rejected with InvalidStateError
        if *suspend_time < 0. {
            promise.reject_error(
                cx,
                Error::InvalidState(Some(String::from("Negative suspend time invalid."))),
            );
            return promise;
        }
        let frames = (*suspend_time * *self.context.SampleRate() as f64) as u64;
        let suspend_frame = frames.div_ceil(FRAMES_PER_BLOCK.0) * FRAMES_PER_BLOCK.0;
        if suspend_frame as f64 / *self.context.SampleRate() as f64 <= *self.context.CurrentTime() ||
            suspend_frame >= self.length as u64 ||
            self.pending_suspend_promises
                .borrow()
                .contains_key(&suspend_frame)
        {
            promise.reject_error(
                cx,
                Error::InvalidState(Some(String::from("Invalid suspend time."))),
            );
            return promise;
        }
        // If there are no pending suspend promises, spin up a background thread to wait on suspend
        // notifications from the rendering thread.
        if self.pending_suspend_promises.borrow().is_empty() {
            self.pending_suspend_promises
                .safe_borrow_mut(cx.no_gc())
                .insert(suspend_frame, promise.to_traced());
            let (sender, receiver) = mpsc::channel();
            self.context
                .audio_context_impl()
                .lock()
                .unwrap()
                .initialize_suspend_notifier(sender);
            let this = Trusted::new(self);
            let task_source = self
                .global()
                .task_manager()
                .dom_manipulation_task_source()
                .to_sendable();
            Builder::new()
            .name("OfflineACSuspendListener".to_owned())
            .spawn(move || {
                while let Ok(suspend_frame) = receiver.recv() {
                    let this = this.clone();
                    task_source.queue(task!(resolve: move |cx| {
                        let this = this.root();
                        let promise = this
                            .pending_suspend_promises
                            .safe_borrow_mut(cx.no_gc())
                            .remove(&suspend_frame)
                            .expect("Should not receive suspend frame with no associated promise").root(cx);
                        this.context.suspend(&promise);
                        // Since suspensions should be scheduled when the context is not running,
                        // if there are no more promises we can close the suspend notifier and thereby
                        // close this background thread.
                        if this.pending_suspend_promises.borrow().is_empty() {
                            this.context.audio_context_impl().lock().unwrap().close_suspend_notifier();
                        }
                    }));
                }
            })
            .unwrap();
        }
        // Send the suspend frame to the rendering thread
        self.context
            .audio_context_impl()
            .lock()
            .unwrap()
            .suspend_at_frame(suspend_frame);
        promise
    }
}
