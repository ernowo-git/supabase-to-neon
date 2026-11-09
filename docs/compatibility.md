# Compatibility

Current alpha support is deliberately narrower than the intended final CLI. Provider documentation was reviewed on 2026-11-09. No live Neon project was used during development.

| Area | Implemented behavior | Gate still required |
| --- | --- | --- |
| Database | PostgreSQL custom dump and transactional restore of explicit schemas | Matching client/server major versions; no unsupported dependencies |
| RLS | Reviewed policy replacements, expected schema, restricted-role read checks | Application session validation and write-path tests |
| Extensions | Reviewed preparation SQL and expected version inventory | Actual provider extension capability proof |
| Auth | Source account inventory, enrollment manifest, approved mapping to existing Neon subjects | Real enrollment, login, recovery, disabled-user and old-token tests |
| Passwords and sessions | Never copied | Users enroll or authenticate again through supported provider flows |
| Storage bytes | Path-style signed S3 list, multipart transfer, full byte and metadata verification | Actual Supabase and Neon endpoint rehearsal |
| Storage access | Public/private intent recorded and checked against source bucket metadata | Provision target visibility and verify public/private application behavior |
| URL references | Read-only proposals for allowlisted text columns and URL prefixes | Review target access and URL validity; apply and reconcile changes separately |
| Realtime, jobs, Edge Functions | Manual action in the plan | Application-specific replacement and tests |
| Recovery | Preserves source and rejects destructive overwrite | Timed recovery rehearsal and post-cutover reconciliation procedure |

The official AWS Rust SDK was not selected for the initial implementation because the provider proof is pending. The CLI uses the existing `object_store` S3 implementation rather than custom SigV4 code. This is a provisional implementation choice, not evidence that every supported SDK operation works on Neon.

Official references:

- [Neon Supabase database migration](https://neon.com/docs/import/migrate-from-supabase)
- [Current Neon Auth migration](https://neon.com/docs/auth/migrate/from-supabase)
- [Neon Object Storage S3 subset](https://neon.com/docs/storage/s3-compatibility)
- [Neon storage credentials](https://neon.com/docs/storage/authentication)
- [Supabase S3 subset](https://supabase.com/docs/guides/storage/s3/compatibility)
- [Supabase backup scope](https://supabase.com/docs/guides/platform/backups)
- [PostgreSQL row security](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)

The older Stack Auth migration blog does not establish password-import support for current Managed Better Auth. Existing policies, dynamic SQL, managed foreign keys, source grants, and external integrations require review even when table rows can transfer. [Reviewed adaptations](adaptations.md) support exact schema-entry exclusions and SQL replacements without changing data column shapes. This does not make arbitrary Supabase schemas automatically compatible.
