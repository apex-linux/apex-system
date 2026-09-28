# Synthetic Apex status documents

Hand-written, **not captured** from a machine. They describe Apex states that bootc's
own fixtures do not cover, in the form bootc 1.16 prints (`org.containers.bootc/v1`,
keys sorted as bootc's canonical JSON writer emits them):

- `apex-staged-download-only.json`: booted, a locked staged update, a rollback, and the
  booted entry's `cachedUpdate` pointing at the staged image.
- `apex-flags-and-drift.json`: every flag that stops automatic updates
  (`rollbackQueued`, `usrOverlay`, an `incompatible` staged entry), a pinned extra
  deployment, and fields and enum values from a hypothetical future bootc.

Replace them with real captures from the qualify VM as soon as those exist.
