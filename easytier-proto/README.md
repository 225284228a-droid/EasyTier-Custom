# Custom protobuf allocation

Official fields keep the numbers and wire types from EasyTier upstream. Every
Custom-only field, including fields in Custom-only messages, uses a number at
or above 50000. Existing official annotation extensions are unchanged.

The 2026-10-16 migration moves each former low Custom number to `number + 50000`.
When that number is already occupied in its message, add 1000 until it is free.
Existing high Custom numbers stay unchanged. `tests/custom-fields.json` records
the complete before/after allocation; `tests/upstream-fields.json` records the
official schema and its source commit. Numbers are scoped to their message.

Update these manifests deliberately when adding fields or merging a newer
official schema. The descriptor tests reject unclassified fields, changes to
official wire types/numbers, and drift in the Custom allocation. Keep field
names stable so TOML, CLI and JSON configuration names are unaffected.

This migration changes the Custom wire protocol. Upgrade Custom Core, Web,
GUI, Go and JavaScript components together. There are no old-tag aliases or
dual reads/writes. The standard upstream protocol remains compatible.

Run `cargo test --locked -p easytier-proto` to check the schema and wire tests.
