# Same-format rollback preparation: next-version tracking

Status: the production correction is being prepared for the next public update.
That update must run the synthetic failed-release rollback phase without the
0.2.0-only scope described below.

## Confirmed behaviour

The 0.2.0 upgrade drill successfully upgrades 0.1.5 to 0.2.0 and verifies the
installed components. Its additional synthetic future release then fails startup
after the swap and attempts to return to the already migrated 0.2.0 installation.
The upgrader unconditionally enters the legacy store conversion helper, whose
target parser rejects public targets >= 0.2.0 because they already read SQLite.
The services remain stopped and the run requests owner intervention.

This is a same-format rollback preparation gap. It is separate from the explicit
one-way 0.1.5 -> 0.2.0 data migration, which cannot automatically roll back to 0.1.5.
The apply worker is executed by the target new release. A real later release must
therefore handle returning to an already migrated target in its own upgrader.

Evidence: [both-platform drill](https://github.com/senxiu-official/cutex/actions/runs/40820321842).
The earlier [0.1.4 -> 0.1.5 drill](https://github.com/senxiu-official/cutex/actions/runs/39294717761)
passed its synthetic failed-release rollback on both platforms.

## Exact 0.2.0 drill scope

Only the frozen product source
`9f73365dd88e3ef4d9e145972f72ea9a28c59d47`, with the verified release label
`0.2.0`, omits the additional synthetic future-release rollback phase. The helper
prints the reason and this tracking link. Other commits and target releases keep
that phase enabled. The actual 0.1.5 -> 0.2.0 upgrade, data/native selection,
component checks and the separate formal signed managed upgrade remain required.
No error is converted into a successful result.

## Completion criteria for the next update

- Treat public targets >= 0.2.0 as already reading the current stores: preparation
  is a no-op, with no legacy conversion, store reads/writes or replacement refusal.
- Leave the existing old-target conversions unchanged.
- Test the actual production host path and the synthetic failed-startup scenario.
- Run the next public release's synthetic rollback phase on both platforms; do not
  extend the frozen 0.2.0 omission to that release.
