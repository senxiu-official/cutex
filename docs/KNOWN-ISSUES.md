# Known issues — cutex 0.2.3

## Worker records require the new backend

The new `worker` service runtime is stored in the service registry. Older versions
cannot read these records. This release therefore declares a one-way record-format
boundary. Keep a backup and explicitly confirm the upgrade; do not downgrade a store
containing worker registrations to an older backend. Removing user records to force a
downgrade is not an upgrade or recovery procedure.

## Claude direct stdio on non-UTF-8 locales

The Claude claude-r46 direct stdio transport uses Python stdout encoding. Non-ASCII
JSON can be unreadable by a UTF-8 client under a non-UTF-8 locale. Set
`PYTHONIOENCODING=utf-8` for direct stdio. Normal cutex-managed Claude sessions use
a loopback WebSocket and do not use this writer.

## External model transport failures

Runtime recovery fixes do not establish or resolve the cause of model failures with
no HTTP response. Check host connectivity and provider status before retrying.

## Host and phone availability

This release provides Windows x64 and Linux x64 hosts. macOS is a desktop client.
No public phone app is included; cutex-mobile remains a separate private dependency.
