# Security

Report vulnerabilities through GitHub private vulnerability reporting when enabled. Do not post credentials, database dumps, user inventories, plans, or run artifacts in public issues. If private reporting is unavailable, open an issue asking for a private reporting channel without including exploit details or sensitive data.

This alpha assumes a trusted operator and a private workstation. A process running as the same OS user can inspect credentials and modify artifacts. The plan digest detects edits; it does not authenticate reviewers. Run-directory permissions and a process lock do not protect against a compromised machine.

Adaptations contain trusted SQL and authorization queries executed using migration credentials. Treat them as executable code, including any `psql` commands they contain. Do not execute unreviewed downloaded artifacts. Preparation, restore, and completion use separate transactions; a failed stage can leave objects from an earlier stage. See `docs/adaptations.md` for review and recovery requirements.

The CLI needs migration privileges. Do not use its target credentials in the application. Test application authorization with restricted runtime roles, including missing identity, disabled accounts, cross-user access, and stale sessions.

Remote PostgreSQL uses verified TLS. S3 uses signed requests over HTTPS. Plaintext connections are limited to loopback test endpoints. PostgreSQL passwords are stored temporarily in a private password file and kept out of subprocess arguments. Provider and PostgreSQL error bodies are suppressed because they can expose credentials or row data.

Run artifacts can contain personal data, function bodies, and database contents. Keep the directory private, use encrypted storage where required, and apply a retention policy. The CLI does not encrypt exports or guarantee secure deletion.

Source and destination writes must be frozen for final migration. The CLI checks visible drift but cannot stop external writers. Target object collision checks assume exclusive migration access; concurrent writes can race them. Multipart uploads interrupted by cancellation may need manual cleanup.
