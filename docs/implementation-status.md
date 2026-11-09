# Implementation status

The project is an alpha implementation of the full-migration plan, not a completed production migration system.

- [x] Standalone Rust package and pinned dependency lockfile
- [x] Nonsecret configuration and protected artifacts
- [x] PostgreSQL, Auth, and storage inspection
- [x] Deterministic plans, dependency blockers, endpoint and tool drift checks
- [x] Run locking and synchronized journal records
- [x] Transactional database transfer for compatible schemas
- [x] Table-content and sequence-state verification
- [x] Account enrollment export and approved application-principal mapping
- [x] S3 multipart transfer, metadata and SHA-256 verification, collision-safe retries
- [x] Operator-attested cutover checks and recovery guidance
- [ ] Reviewed SQL adaptations and restore ordering for managed-schema dependencies
- [ ] Target extension preparation and policy translation
- [ ] Supported automated Neon enrollment API proof
- [ ] Real Supabase and Neon disposable-project rehearsal
- [ ] Automated URL-reference rewrite previews
- [ ] Provider-specific application authorization and storage access proofs
- [ ] Disk exhaustion, credential expiry, throttling, and cancellation failure matrix
- [ ] Measured production-volume limits and recovery objectives
- [ ] Signed release binaries and crates.io publication

Publishing source as an alpha does not satisfy the unchecked provider and production release gates.

## Local validation

The local check suite passes formatting, Clippy with warnings denied, and nine safety tests. The synthetic Docker rehearsal covers database transfer, sequence values, approved active-account mappings, disabled-account denial, S3 multipart transfer, metadata, Unicode and empty objects, safe retries, blocked plans, stale evidence, and altered destination data. It uses fixture Auth tables and MinIO rather than real managed services.

The pinned dependency set passes Cargo Audit with no reported vulnerabilities or informational advisories, and Cargo Deny license checks. Checks are point-in-time results; CI reruns them for changes.
