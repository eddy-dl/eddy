use tokio::sync::mpsc;

use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::io::{WrittenPrefixes, drain_write_chunks, record_in_flight};

#[allow(clippy::too_many_arguments)]
pub(in crate::engine::http::concurrent_download) async fn cancel_and_persist(
    executor: HttpSegmentRequestExecutor,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    global_limiter: Option<&RateLimiter>,
    ctrl_file: &mut Option<ControlFile>,
    completed_bytes: u64,
) -> Result<()> {
    cancel_and_persist_segments(
        executor,
        write_rx,
        writer,
        limiter,
        global_limiter,
        ctrl_file,
        completed_bytes,
        None,
    )
    .await
}

/// Like [`cancel_and_persist`], also saving the progress of segments still
/// downloading as in-flight pieces, so a resume continues inside them.
#[allow(clippy::too_many_arguments)]
pub(super) async fn cancel_and_persist_segments(
    executor: HttpSegmentRequestExecutor,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    global_limiter: Option<&RateLimiter>,
    ctrl_file: &mut Option<ControlFile>,
    completed_bytes: u64,
    mut in_flight: Option<(&ConcurrentSegmentManager, &mut WrittenPrefixes)>,
) -> Result<()> {
    executor.cancel().await;
    drain_write_chunks(
        write_rx,
        writer,
        limiter,
        global_limiter,
        in_flight.as_mut().map(|(_, written)| &mut **written),
        " while cancelling",
    )
    .await?;
    writer.flush().await.map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Flush failed while cancelling: {error}"
        )))
    })?;

    if let Some(control_file) = ctrl_file {
        control_file.update_completed_length(completed_bytes);
        if let Some((manager, written)) = in_flight {
            record_in_flight(control_file, manager, written);
        }
        if let Err(error) = control_file.save().await {
            tracing::warn!("Control file save on pause/remove failed: {error}");
        }
    }
    Ok(())
}
