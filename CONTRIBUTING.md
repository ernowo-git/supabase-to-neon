# Contributing

Run `just check` before proposing changes. Data-transfer changes also need `just rehearse`. Use synthetic fixtures only. Describe supported source and target versions and reproduce failures without credentials or customer data.

Changes to authorization, identity mapping, restore ordering, or retry behavior need a failure case that would otherwise cause data loss or grant access. Keep provider operations tied to official documentation and record untested assumptions in `docs/compatibility.md`.
