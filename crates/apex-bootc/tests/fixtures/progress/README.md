# Progress stream fixture

`upgrade-download-only.jsonl` is **synthetic**: the sequence of events
`bootc upgrade --download-only --progress-fd 3` writes, reconstructed from bootc's
emitting code (`crates/lib/src/deploy.rs` and `crates/lib/src/progress_jsonl.rs`, bootc
`main` at commit `41049ccada2bf4468b9087d45832f9168d458ef2`; bootc is MIT OR Apache-2.0)
in bootc's canonical JSON form (sorted keys, one event per line).

The run it describes:

1. Two layers (50 MiB) are already present; three (100, 120 and 80 MiB) must be fetched.
2. The first layer finishes. The fetch of the second fails at 60 MiB with a retryable
   blob error. As in `pull_from_prepared`, the failed attempt still ends with the two
   `importing` events (0 of 1 from `handle_layer_progress_print`, then 1 of 1) before
   the error reaches `retry_pull_operation`, which sends the `pull-retry` event.
3. `pull_once` starts over: `prepare_for_pull` now finds the first layer committed, so
   `bytesCached` and `stepsCached` grow, `bytesTotal` and `stepsTotal` cover only the
   two missing layers, and a new layer printer counts `bytes` from 0 again. The second
   layer is fetched from its start.
4. Import and the four staging steps follow. The last staging event lists the finished
   `cleanup` subtask twice, as `stage` pushes it and then chains it again.

It also contains one non-JSON line and one event of an unknown type, which the parser
must skip.

Replace it with a capture from the qualify VM when one exists (a pull retry can be
provoked by dropping the connection to the registry during the download).
