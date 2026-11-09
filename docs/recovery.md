# Recovery

Before destination writes open, stop the destination application and return traffic to the frozen source if needed. Recheck source connectivity, sessions, storage, and background jobs before reopening writes.

After destination writes open, changing connection strings back can lose new records and objects. Stop writes on both systems. Capture destination changes, reconcile them through the application's recovery process, and choose the recovery system deliberately. This CLI has no reverse replication.

Database restore uses one transaction. A failure still requires target inspection, a verified empty target, and a new run directory. Never drop schemas automatically to make a retry pass. Keep the failed run for diagnosis without sharing its contents publicly.

Auth and storage retries inspect existing remote results. Principal mismatches, object collisions, and stale plans stop the run. Do not edit a plan or checkpoint to suppress a failure. Generate a new inventory and plan after resolving drift.

An interrupted multipart transfer can leave incomplete upload parts. Inspect and remove only migration-owned unfinished uploads through supported provider tools. A completed object is re-read and verified on resume.
