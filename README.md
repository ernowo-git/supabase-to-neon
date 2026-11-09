# supabase-to-neon

A Rust CLI for inspecting and rehearsing a Supabase exit to Neon Postgres, Neon Auth, and Neon Object Storage.

This is an early alpha. It transfers compatible application schemas, records verified identity mappings after account enrollment, and streams S3 objects with independent SHA-256 verification. It does not import Supabase passwords or sessions. Current Neon documentation requires a separate account transition. [Neon Auth migration](https://neon.com/docs/auth/migrate/from-supabase)

Read [compatibility](docs/compatibility.md) before using real data. Existing RLS policies, managed-schema references, cross-schema foreign keys, partitions, and non-default extensions currently block database transfer. No production compatibility or zero-downtime guarantee is claimed.

## Build and check

Install Rust through rustup and install `just`. The pinned toolchain and Cargo.lock define the build.

```sh
just build
just check
just cli --help
```

Linux and macOS are the intended platforms. Windows is unsupported because protected run directories depend on Unix permissions. PostgreSQL 17 or newer client utilities are required for system certificate support. Client utility and server major versions must match in this alpha.

## Inspect and plan

```sh
just cli init
```

Edit `migration.toml` with explicit application schemas, project identifiers, bucket mappings, endpoints, and credential environment variable names. Supply connection strings and storage credentials through your shell or secret manager. The CLI does not load `.env` files. It does not provision projects, Auth, or bucket visibility.

Use direct PostgreSQL connections. Remote database URLs require `sslmode=verify-full`; `sslrootcert=system` is the default. A custom CA file may be supplied through the URL's `sslrootcert` option. HTTP and disabled database TLS are allowed only for loopback test services. Source credentials need complete read access, including bypassing source RLS for export; target migration credentials need DDL privileges. Runtime application credentials must remain separate and restricted.

```sh
just cli doctor
just cli inspect --out inventory.json
just cli plan --inventory inventory.json --out plan.json
just cli auth-export --plan plan.json --out enrollment-source.json
```

Inventory and plans may contain personal data and SQL function bodies. They are written with private permissions and never overwritten. Protect and dispose of these artifacts as database exports. A plan contains a digest of its inventory, nonsecret configuration, endpoint identities, and tool versions. This detects accidental edits and drift; it is not a cryptographic signature from a trusted reviewer.

The source SQL inspection reports known dependencies. Dynamic SQL, external services, jobs, and application behavior still require human review. Project IDs are operator labels; endpoint fingerprints, rather than provider account APIs, bind the inspected connections.

## Rehearse a transfer

Enroll users through supported Neon Auth flows. Create a private enrollment JSON file using the shape in [the example](examples/enrollment.json). Every active source user needs an approved mapping to an existing destination Auth account. Disabled source users remain unmapped. An approver and evidence reference attest verified enrollment; email equality never creates a mapping automatically.

The Auth phase creates application-owned `migration_identity.principal` and `migration_identity.provider_identity` tables. They preserve source UUIDs and deny public access through RLS and grants. Integrating these tables into application authorization and foreign keys is a separate reviewed application migration. The CLI does not write to managed Auth tables, send invitations, or verify proof-of-control documents itself.

Freeze database, Auth, storage, and background writes. Review the plan and pass its digest explicitly:

```sh
just cli apply --plan plan.json --phase prepare --run .runs/rehearsal --dry-run
just cli apply --plan plan.json --phase prepare --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen
just cli apply --plan plan.json --phase database --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen
just cli apply --plan plan.json --phase auth --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen --enrollment enrollment.json
just cli apply --plan plan.json --phase storage --run .runs/rehearsal --ack-plan PLAN_DIGEST --writes-frozen
just cli verify --plan plan.json --enrollment enrollment.json --out verification.json
just cli report --run .runs/rehearsal
```

Database restore uses one PostgreSQL transaction and does not overwrite an existing application schema. A failed database run needs a new run directory and a verified empty target. Auth and storage retries inspect remote results before proceeding. Storage refuses differing existing objects, never deletes keys, and re-reads bytes and metadata on resume. Do not allow concurrent writers to the destination.

The run journal records completed operations. An interrupted step is incomplete until checked again. SIGINT stops the CLI and kills its PostgreSQL child process. Interrupted multipart uploads may leave unfinished upload parts that need provider-side cleanup.

## Verify and cut over

Verification compares table row digests, schema definitions, sequence state, principal mappings, object key sets, metadata, and content digests. It reads complete table and object contents. Budget time and egress accordingly.

Use [cutover evidence](examples/evidence.json) to record application tests, restricted runtime RLS checks, storage access tests, the write freeze, and recovery rehearsal. Replace every example value with a reference to actual evidence. Set `recorded_at_unix` to the current Unix time. Evidence must be recorded within the previous hour.

```sh
just cli cutover-check --plan plan.json --enrollment enrollment.json --evidence evidence.json --out readiness.json
```

This command reruns data checks and records operator attestations. It does not validate external documents or switch traffic. The provider compatibility gate and application release review remain necessary. See [cutover](docs/cutover.md) and [recovery](docs/recovery.md).

## Synthetic integration rehearsal

Install `tests/requirements.txt` in a Python virtual environment and allow Docker access, then run:

```sh
just rehearse
```

The script creates isolated PostgreSQL and MinIO containers, exercises the CLI with synthetic records and objects, and removes those containers afterward. It never reuses existing databases or storage. PostgreSQL tools run in the disposable container. Set `REHEARSAL_POSTGRES_IMAGE` and `REHEARSAL_S3_IMAGE` to image digests for reproducible infrastructure.

MinIO and fixture Auth tables test protocol behavior, not Neon compatibility. Real Neon Auth and Object Storage need a disposable provider rehearsal before any production claim.

## Development status

Implemented: config checks, inspection, deterministic plans, protected artifacts, run locking, journal records, compatible database transfer, enrollment manifests and principal mappings, S3 transfer and content verification, technical readiness checks, and local failure tests.

Still required for the broader migration plan: reviewed schema adaptations and restore ordering for managed dependencies, extension preparation, automated provider-supported enrollment where available, a real Neon compatibility rehearsal, application access-policy proofs, broader failure injection, and measured operational limits. See [implementation status](docs/implementation-status.md).

MIT licensed. See [security](SECURITY.md) and [contributing](CONTRIBUTING.md).

Commands return 0 on success, 1 on a failed check or operation, 2 on invalid arguments, and 130 on interruption. JSON diagnostics go to stderr. No service mutation occurs in dry-run mode.
