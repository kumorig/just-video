Vendored smb 0.12.1 (MIT, see LICENSE; upstream https://github.com/afiffon/smb-rs) with these changes for Just Video:
`Worker::receive` (src/connection/worker/worker_trait.rs) adds the credits granted
in interim STATUS_PENDING responses to the final response before credit accounting.
Samba grants an async request's credits in the interim response and none in the
final one, so unpatched every read that goes async (common under load: many 1 MiB
reads in flight, slow disks) leaked its credit charge. After a few thousand reads the
connection had no credits left and every request waited forever ("The server stopped
sending data"). Upstream candidate.

`QueryDirectoryStream` (src/resource/directory.rs) uses `notify_one` to request
the next directory batch, and the producer no longer sends notifications to
itself. `notify_waiters` lost the wakeup when the consumer drained a batch before
the producer began waiting, leaving normal browsing stuck until the 15-second
request deadline. `notify_one` retains a permit for that ordering. Regression
tests force both the early-consumer and already-waiting orderings. Upstream candidate.
