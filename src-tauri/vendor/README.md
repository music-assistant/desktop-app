# Temporary Sendspin diagnostics

`sendspin/` is the tracked source archive of Sendspin/sendspin-rs commit
`acde529c3d087ddea66680800c1ce6f0df4c6b08` (v0.3.8), the version pinned by
desktop release 0.7.4. Its original license file is included unchanged.

This diagnostic branch patches that dependency to the local copy so a reporter
can build the timing instrumentation from one checkout. Changes to the library
are limited to bounded clock-sync timing logs and their tests. The upstream
README's redundant heading underline and trailing whitespace are also removed
to pass the repository's whitespace checks. Socket options,
probe cadence, sample acceptance thresholds, and playback policy are unchanged.

This is temporary investigation code for desktop issue #247, not a dependency
upgrade or a proposal to maintain a permanent fork.
