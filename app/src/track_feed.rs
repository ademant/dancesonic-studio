//! Feeds a [`TrackSource`] to GStreamer through `appsrc` (#33).
//!
//! The player gets a URI of the form `appsrc://track/<n>`. When playbin
//! creates the `appsrc` for it, [`Feeds::attach`] configures it for random
//! access. `appsrc` asks for data from its own streaming thread: `seek-data`
//! sets the offset, `need-data` reads there and pushes. Both run on that one
//! thread, so a push can never race a seek.
//!
//! A read may wait for the network. When GStreamer flushes (a seek, a stop),
//! the source pad turns flushing and the read gives up at once, so a seek
//! never waits on the network.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSrc, AppStreamType};
use studio_core::queue::SongId;
use studio_core::track_source::{SourceError, TrackSource, Tracks};

/// URI prefix of the player's tracks.
const PREFIX: &str = "appsrc://track/";

/// Bytes read and pushed at a time.
const CHUNK: usize = 64 * 1024;

/// Sources prepared but not yet picked up by playbin. A prepared next track
/// that is never played is dropped once newer ones push it out.
const MAX_PENDING: usize = 8;

/// The track sources of one player. Cloning shares them.
#[derive(Clone, Default)]
pub(crate) struct Feeds {
    pending: Arc<Mutex<BTreeMap<u64, TrackSource>>>,
    counter: Arc<AtomicU64>,
}

impl Feeds {
    /// A URI for `song`, read from `tracks`. Does not block.
    pub(crate) fn uri(&self, tracks: &Tracks, song: &SongId) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.insert(n, tracks.open(song));
        while pending.len() > MAX_PENDING {
            pending.pop_first();
        }
        format!("{PREFIX}{n}")
    }

    /// Called from playbin's `source-setup`: configures `element` when it is
    /// an `appsrc` for one of our URIs.
    pub(crate) fn attach(&self, element: &gst::Element) {
        let Some(appsrc) = element.downcast_ref::<AppSrc>() else {
            return;
        };
        let Some(uri) = appsrc
            .dynamic_cast_ref::<gst::URIHandler>()
            .and_then(|h| h.uri())
        else {
            return;
        };
        let Some(n) = uri.strip_prefix(PREFIX).and_then(|n| n.parse::<u64>().ok()) else {
            return;
        };
        let Some(source) = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&n)
        else {
            tracing::warn!("playbin asked for a track that is no longer prepared");
            return;
        };
        appsrc.set_stream_type(AppStreamType::RandomAccess);
        appsrc.set_format(gst::Format::Bytes);

        let feed = Arc::new(Feed {
            source: Mutex::new(source),
            offset: AtomicU64::new(0),
            sized: AtomicBool::new(false),
        });
        let seek = Arc::clone(&feed);
        let callbacks = gstreamer_app::AppSrcCallbacks::builder()
            .need_data(move |src, _| feed.serve(src))
            .seek_data(move |_, offset| {
                seek.offset.store(offset, Ordering::SeqCst);
                true
            })
            .build();
        appsrc.set_callbacks(callbacks);
    }
}

/// One track being played.
struct Feed {
    source: Mutex<TrackSource>,
    offset: AtomicU64,
    sized: AtomicBool,
}

impl Feed {
    /// Answers `need-data`: reads at the offset and pushes, or ends the stream.
    fn serve(&self, src: &AppSrc) {
        let pad = src.static_pad("src");
        // A flush (seek or stop) cancels a read that waits for the network.
        let cancel = || {
            pad.as_ref()
                .is_none_or(|p| p.pad_flags().contains(gst::PadFlags::FLUSHING))
        };
        let mut source = self.source.lock().unwrap_or_else(PoisonError::into_inner);
        if !self.sized.load(Ordering::SeqCst) {
            match source.len(&cancel) {
                Ok(len) => {
                    src.set_size(i64::try_from(len).unwrap_or(i64::MAX));
                    self.sized.store(true, Ordering::SeqCst);
                }
                Err(SourceError::Cancelled) => return,
                Err(error) => return fail(src, &error),
            }
        }
        let offset = self.offset.load(Ordering::SeqCst);
        let mut buf = vec![0u8; CHUNK];
        match source.read_at(offset, &mut buf, &cancel) {
            Ok(0) => {
                // Refused while flushing; the next request follows the seek.
                let _ = src.end_of_stream();
            }
            Ok(read) => {
                buf.truncate(read);
                let mut buffer = gst::Buffer::from_mut_slice(buf);
                if let Some(buffer) = buffer.get_mut() {
                    buffer.set_offset(offset);
                }
                if src.push_buffer(buffer).is_ok() {
                    self.offset.store(offset + read as u64, Ordering::SeqCst);
                }
            }
            Err(SourceError::Cancelled) => {}
            Err(error) => fail(src, &error),
        }
    }
}

/// Reports a read error to the pipeline. The text carries no URL.
fn fail(src: &AppSrc, error: &SourceError) {
    tracing::warn!(%error, "a track could not be read");
    let message = gst::message::Error::builder(gst::ResourceError::Read, &error.to_string())
        .src(src)
        .build();
    // Ignored: without a bus there is nobody to tell.
    let _ = src.post_message(message);
}
