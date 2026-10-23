use std::collections::HashMap;

use tokio::sync::mpsc;

use crate::engine::concurrent_segment_manager::{ConcurrentSegmentManager, SegmentStatus};
use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::super::acquire_download_tokens;

/// How far each requested segment has been handed to the writer without a
/// gap, so a pause keeps the progress of segments still downloading.
pub(in crate::engine::http::concurrent_download) struct WrittenPrefixes {
    piece_length: u64,
    ends: HashMap<u64, u64>,
}

impl WrittenPrefixes {
    pub(super) fn new(piece_length: u32) -> Self {
        Self {
            piece_length: u64::from(piece_length).max(1),
            ends: HashMap::new(),
        }
    }

    /// A request for a segment starts at `offset`; bytes before it are
    /// already on disk.
    pub(super) fn start(&mut self, offset: u64) {
        let end = self.ends.entry(offset / self.piece_length).or_insert(offset);
        *end = (*end).max(offset);
    }

    /// A chunk was written. Ranges carry one segment's bytes in order, so a
    /// chunk extends the prefix when it starts inside it.
    pub(super) fn record(&mut self, offset: u64, length: usize) {
        if let Some(end) = self.ends.get_mut(&(offset / self.piece_length))
            && offset <= *end
        {
            *end = (*end).max(offset + length as u64);
        }
    }

    fn prefix(&self, index: usize) -> u64 {
        let start = index as u64 * self.piece_length;
        self.ends
            .get(&(index as u64))
            .map_or(0, |end| end.saturating_sub(start))
    }
}

/// Persist each unfinished segment's written prefix as an in-flight piece.
/// The writer must have been flushed first.
pub(in crate::engine::http::concurrent_download) fn record_in_flight(
    control_file: &mut ControlFile,
    manager: &ConcurrentSegmentManager,
    written: &WrittenPrefixes,
) {
    for index in 0..manager.num_segments() {
        if manager.segment_status(index) != Some(SegmentStatus::Done) {
            let completed = manager
                .segment_completed_length(index)
                .max(written.prefix(index));
            control_file.set_in_flight_prefix(index, completed);
        }
    }
    control_file.recount_completed();
}

pub(super) async fn drain_write_chunks(
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    global_limiter: Option<&RateLimiter>,
    mut written: Option<&mut WrittenPrefixes>,
    error_context: &str,
) -> Result<()> {
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        acquire_download_tokens(limiter, global_limiter, data.len()).await;
        let length = data.len();
        writer.write_bytes_at(offset, data).await.map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Write failed{error_context}: {error}"
            )))
        })?;
        if let Some(written) = written.as_deref_mut() {
            written.record(offset, length);
        }
    }
    Ok(())
}
