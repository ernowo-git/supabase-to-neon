# Cutover

Use a disposable target first. Establish numeric downtime, recovery, and verification limits from the rehearsal. Current alpha restrictions in compatibility.md must be satisfied before a data transfer.

1. Provision target database, Auth, and buckets. Verify region and service availability. Keep application runtime credentials separate from migration credentials.
2. Adapt and test application identity, RLS, queries, uploads, URL generation, and external integrations. Complete enrollment of required active accounts and verify approved mappings. Configure SMTP and OAuth callbacks independently.
3. Freeze database, Auth, object, queue, scheduled, and integration writes. Record the freeze owner and time. The CLI flag acknowledges this action; it does not perform it.
4. Generate a fresh inventory and plan. Review blockers and manual actions. Keep exported data and artifacts private.
5. Run prepare, database, Auth, and storage phases. Run verify. Inspect the journal and resolve every error before proceeding.
6. Exercise the application against the destination with restricted runtime roles. Test public/private storage, login, logout, recovery, permissions, and stale token rejection. Record actual evidence in a private file with the current plan digest.
7. Run cutover-check. It reruns technical checks and records operator attestations. Its success does not replace the provider compatibility gate or application release approval.
8. Switch application configuration using the deployment system. Check representative flows before opening destination writes. Record when the rollback boundary is crossed.
9. Observe errors, authorization failures, delayed writers, and media delivery. Keep the source frozen and intact through the agreed observation period. Retire credentials and resources separately after reconciliation.
