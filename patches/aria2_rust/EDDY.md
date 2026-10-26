# aria2_rust, patched

aria2-core and aria2-protocol from <https://github.com/balovess/aria2_rust/>
at e75732eb21d83bbf4bf016037dbf02df7b91a0c9, with a fix for resuming HTTP
downloads and fixes for starting a torrent again (Cargo.toml's `[patch]`
points here). Only these two crates of
the workspace are kept; the rest is unchanged upstream code (GPL-3.0, see
LICENSE).

## The bug

A download with an output name (`out`) starts sequentially, and its control
file is one piece the size of the file. Every later run is concurrent but
adopted that piece length, so the rest of the file was a single range that
never completed before a pause. `completed_length` stayed at the first
pause, and each resume after went back there.

## The fix

- `segment/plan.rs`, `pipeline/setup.rs`: a one-piece (sequential)
  checkpoint's layout isn't adopted; the fixed piece size is used.
- `segment/resume.rs`, `pipeline/setup.rs`: that checkpoint's
  `completed_length` is restored as a prefix into the new layout.
- `segment/resume.rs`: pieces restored from a prefix are marked in the new
  control file's bitfield, so the next resume keeps them.
- `filesystem/resume_helper.rs`: a sequential run over a split download's
  checkpoint continues from the unbroken run of finished pieces at the
  start, not `completed_length` (which counts pieces with gaps between).

A pause kept only finished pieces, so a split download lost the pieces
still downloading (paused at 5.6 MB, resumed at 4 MB). A failed attempt
kept even less: the sequential fallback's gaps never reached the control
file, and a retry that runs sequentially (its HEAD failed) wrote only a
prefix length into the split checkpoint, which the next concurrent resume
ignores for the bitfield.

- `filesystem/control_file.rs`: helpers for HTTP in-flight pieces (aria2's
  16 KiB block bitfield, prefix only, never lowered), and
  `record_completed_range`/`record_completed_prefix`, which mark covered
  pieces done and the partly covered one in flight.
- `concurrent_download/segment/io.rs`, `segment.rs`, `cancel.rs`,
  `finalize.rs`: each segment's contiguously written bytes are tracked and
  saved as in-flight pieces on pause/remove, on a failure, on the fallback
  and on periodic saves. Periodic saves flush the write cache first.
- `concurrent_segment_manager/segment_lifecycle.rs`, `segment/resume.rs`:
  in-flight pieces are restored, so their ranges start at offset + done.
  `segment/resume.rs`, `pipeline/setup.rs`: a checkpoint with in-flight
  pieces isn't read as a `completed_length` prefix.
- `filesystem/resume_helper.rs`: the sequential offset also takes the
  in-flight start of the piece after the leading run.
- `sequential_download/response_body.rs`, `download_flow.rs`: sequential
  progress goes into a split checkpoint as pieces; a resume from offset 0
  no longer truncates the file; a fresh run drops the old checkpoint.
- `sequential_download/gap_download.rs`: finished and partial gaps are
  recorded in the control file (after a flush) instead of zero-filled; the
  control file is removed on completion. A gap whose body ends early fails
  the attempt (it used to complete the download with the gap missing).
- `download_engine/progress.rs`: progress stepping back (into a fallback)
  no longer overflows the speed delta.

A sequential stream that broke left the checkpoint at the last periodic
save (4.5 MB back), and the retries in the group reused the first
attempt's resume state. A 429 from too many connections failed the group:
each 429 lowered the target by one per round, then used up the range's
retries, and three in a row disabled the only mirror (the stall that
dropped into the gap fallback on the phone).

- `sequential_download/response_body.rs`: a stream or write error saves
  the exact position before returning, like a pause.
- `sequential_download/mod.rs`: each retry in the group detects the resume
  state again.
- `segment_downloader/downloader/{mod,streaming}.rs`, `request_executor*`:
  a 429/503's Retry-After (seconds or HTTP date, capped at 5 min) reaches
  the scheduler.
- `adaptive_concurrency.rs`, `concurrent_download/segment.rs`: a 429/503
  drops the target at once to the ranges being served, pauses new ranges
  for Retry-After or 1 s doubling to 30 s with jitter, and requeues the
  range without using its retries; in-flight ranges go on. After 4 × target
  successes the target goes up by one. Give up only after 5 min without a
  finished range, or max-tries backoffs before any range finished (pieces
  and in-flight bytes are saved).
- `concurrent_segment_manager/mod.rs`, `segment/plan.rs`: the only mirror
  of a single-source download is never disabled; each range's own retry
  limit ends a failing download.
- `download_command/execute.rs`: an error response's Content-Length (a
  429 HEAD) isn't taken as the file size.
- `download_command/execute.rs`: a 429/503 to the range probe a run starts
  with backs off (1 s doubling to 30 s with jitter, up to 5 min) without
  spending its retries. Servers that limit requests per second (hetzner's
  speed-test mirror, no Retry-After) otherwise failed a resumed download
  at once.

## Starting a torrent again

Eddy starts a torrent again to change its options, to pick other files
("Choose files") while it runs, and to fetch more of a finished one's files.
All of these failed or started over, in one engine:

- A finished torrent's command isn't dropped at once (nor, with
  keep-alive, soon), and it held the torrent's info-hash route and its
  files' reservation until it was: a new download of the same torrent
  failed with "info-hash route is already registered" or "is being
  downloaded by other command". `download/execute/finalization.rs`
  releases both first thing on completion.
- A removed (or finished) download winds down for a moment (its trackers
  told it stopped) still holding its route, BtRegistry entry and file
  reservation, so a restart straight after failed ("InfoHash … is already
  registered"). `download/execute/peer_session.rs` and `command.rs` wait
  for them up to 10 s (250 ms steps); `registry.rs` gains
  `unregister_bt_download` to undo a registration while it waits for the
  files.
- A checkpoint was only trusted if every file of the torrent existed. With
  some files picked and no pre-allocation, unpicked files (and picked ones
  not reached yet) never do, so every restart threw the progress away and
  downloaded from 0. `persistence/checkpoint.rs` gains `open_checked`,
  which tests the checkpoint's bitfield; `download/execute/environment.rs`
  and `state.rs` require only the files holding finished pieces.

Checked on Sintel's torrent (resume-test `repick`): paused at 41 MB with two
files unpicked, one of them picked and restarted, it carries on from 41 MB
and every file matches a whole download; a finished one gets the newly
picked file into its (renamed) folder with the others untouched.

Upstream issue: <https://github.com/balovess/aria2_rust/issues/78>. Drop this
copy once a fix is released.
