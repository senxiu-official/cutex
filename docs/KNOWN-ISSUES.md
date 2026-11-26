# Known issues — 0.2.6

> **0.2.6 upgrade boundary:** after 0.2.6 starts, rollback to 0.2.5 is unsupported. Keep a complete, consistent offline backup from before upgrading and the upgrade-run backup. The old 0.2.5 managed backup excludes personal-assistant and schedule stores; preserve consistent snapshots separately for data created by 0.2.6. Older rollback instructions below describe earlier release boundaries.
>
> **0.2.6 升级边界：**0.2.6 启动后，不支持退回 0.2.5。请保留升级前完整且一致的离线备份和升级程序备份；旧版备份不含个人助理与任务日程存储，0.2.6 新增数据须另留这两份存储的一致快照。下文旧版回退步骤仅描述更早的版本边界。


For cutex 0.2.6, the public Native runtime is 0.172.0-r70. On Linux it requires a capable system `bwrap`, `pidfd_open` (Linux 5.3 or newer), and a host policy permitting sandbox startup. It does not bundle bubblewrap. AppArmor restrictions on user namespaces can prevent transparent TCP proxy startup. Missing capabilities refuse startup without silently weakening sandboxing.


# Known issues — cutex 0.2.4

## Worker records require the new backend

The new `worker` service runtime is stored in the service registry. Older versions
cannot read these records. This release therefore declares a one-way record-format
boundary. Keep a backup and explicitly confirm the upgrade; do not downgrade a store
containing worker registrations to an older backend. Removing user records to force a
downgrade is not an upgrade or recovery procedure.

## External model transport failures

Runtime recovery fixes do not establish or resolve the cause of model failures with
no HTTP response. Check host connectivity and provider status before retrying.

## Host and phone availability

This release provides Windows x64 and Linux x64 hosts. macOS is a desktop client.
No public phone app is included; cutex-mobile remains a separate private dependency.

## Manual return to 0.2.3

On a manual return to 0.2.3, dismissals still waiting to apply are not applied there; dismiss those rows again in that version. The records are retained and used again after returning to 0.2.4.
