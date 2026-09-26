# Per-pane geometry for control clients

Companion to the [Rootshell font-size fix](https://github.com/dannyking/rootshell/tree/fix/herdr-zoom-pane-resize), reported in [Rootshell #533](https://github.com/kitknox/rootshell/issues/533).

Prepared for Rootshell fork release v0.1.7, based on v0.1.6. Danny King's original server commit `5971999e` and companion client commit `71287e2e` are preserved in history. This is a fork feature; no implementation PR has been opened against upstream Herdr.

## Behavior

When a native client gives panes different font sizes, one tab-wide cell size cannot describe every terminal grid. Control protocol 3 advertises optional `pane_geometry` and accepts `tab.set_pane_geometry`:

- Existing tab geometry fields retain their meaning and `claim` semantics.
- `panes` maps pane IDs to `cols`, `rows`, `cell_width_px`, and `cell_height_px`. Grids must have at least 4 columns and 2 rows, matching the runtime minimum; smaller requests are rejected before changing ownership or stored geometry.
- The owning connection's map sizes the terminal runtimes. Other connections can store their preferences without claiming ownership.
- Protocol-3 layouts carry optional `terminal_size`; `rect` still describes split placement.
- Protocol-1/2 viewers receive a compatible arrangement of the actual terminal grids. Taking control with the old geometry method restores layout-derived sizing.

Shared attachments stay shared. There is still one geometry owner per tab, not a separately sized terminal for each viewer. The new request is validated before stored geometry changes. The generated API schema is updated; frozen endpoint fixtures are unchanged.

## Validation and remaining work

- Human local macOS A/B test: the pre-fix Rootshell sizing code reproduced grey gaps after font zoom; the modified client did not, against this same server. Both clients included an unrelated local discovery workaround.
- 24 control-stream tests, 50 schema tests, and 228 endpoint compatibility tests passed.
- Release build, formatting/Clippy, and six UI hot-path architecture tests passed.
- Six render-scale benchmarks passed on both the base and candidate. Background combined pipeline medians for 1/15 panes: base 973/1033 microseconds, candidate 998/1061. Active server surface medians: base 603/749, candidate 574/769. These single runs overlapped other builds and are supporting evidence, not precise regression bounds.

Fork integration validation: 3,533 enabled Rust tests passed (6 skipped), along with formatting, Clippy, maintenance/integration checks, 7 documentation tests, and the native release build. Local Windows cross-lint remains blocked by the missing Windows SDK; Linux/Windows release builds run in the fork release workflow. Rootshell passed 447 tests and its simulator build. The user confirmed split-pane font resizing on iPad after restarting into the updated server. Full `just ci` and manual multi-client testing remain pending. Automated control-stream coverage includes shared viewers, legacy projections, zoom/unzoom, ownership handoff, and rejected requests leaving state unchanged.

Implementation and tests were AI-assisted; the contributor performed the visual A/B comparison. Both the companion Rootshell client and this server change are required to correct the reported behavior.
