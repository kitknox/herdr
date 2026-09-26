# Rootshell fork releases

Rootshell builds are published by `kitknox/herdr` using independent version tags:
`rootshell-v0.1.0`, `rootshell-v0.1.1`, and so on.

## Install from official Herdr on macOS or Linux

Paste this into a terminal:

```sh
curl -fsSL https://github.com/kitknox/herdr/releases/download/rootshell-channel/install.sh | sh
```

This permanent URL selects the current Rootshell version from the fork's update
manifest. The installer verifies the asset checksum, installs into
`~/.local/opt/herdr-rootshell/bin`, and adds that directory to the front of PATH
in your Bash or Zsh startup files. It preserves the official installation,
including Homebrew, mise, and Nix package files. Open a new terminal afterward
and run `herdr --version`; it should include `rootshell` and the installed fork version.

Existing servers keep running their previous binary. Save work before restarting
them: stopping a server terminates its pane processes. From a terminal outside
Herdr, run `herdr server stop`, then `herdr`. For a named session, use
`herdr session stop NAME`, then `herdr --session NAME`.

## Future updates

From outside Herdr, run:

```sh
herdr update
```

Rootshell binaries use the fork's update manifest regardless of the shared
stable/preview configuration. Updates compare Rootshell versions, verify
checksums, and retain this fork update source. `herdr channel set stable` does
not migrate a Rootshell binary to the official distribution. The ordinary
stable/preview channel labels describe shared configuration, not the fork's
compiled update source.

## Return to official Herdr

Remove the lines marked `# rootshell-herdr` from your shell startup files and
open a new terminal. Update the preserved official install using its normal
installer or package manager, then restart the server after saving work.
Merging the feature upstream does not automatically migrate fork users. We will
announce an official version containing the feature and migration instructions.

## Control protocol 3

Rootshell v0.1.7 adds `pane_geometry` and `tab.set_pane_geometry` for independent
per-pane terminal grids. A compatible client can change one pane's font size
and resize its terminal grid without changing the other panes. Protocol-3
layouts include `terminal_size` independently from the split-layout `rect`.
Protocol-1/2 clients remain supported; clients gate the new method on the
advertised feature. See [per-pane geometry](control-pane-geometry.md).

The companion Rootshell client change is being prepared for an upcoming app
release. Both client and server support are needed for independent pane sizing.
Install the update completely before restarting the server, then reconnect the
client. An already-running server retains its previous protocol and features.

Thanks to Danny King (@dannyking), whose original client and server commits are
preserved in the repositories.

## Control protocol 2

Earlier fork releases advertise `terminal_control_stream: 2` and a
`control_features` list in `ping` and `control.open` capabilities. Clients gate
on the feature names, not the number: `shared_attach` (several streams hold one
pane), `geometry_ownership` (`tab.set_geometry` with `claim`, `tab.claim_geometry`,
input claims), `geometry_controller` (layouts name the tab's owner and
`tab.geometry_changed` reports changes), `control_list`, `client_identity`
(`control.open` takes a `client` object), `query_authority` (`terminal.authority`
records), `event_drain`, `event_gap`, and `auto_input` (`terminal.input` with
`auto: true` for the emulator's own replies: forwarded only from the query
authority, never a geometry claim).

A client opts in by sending `client.protocol: 2` on `control.open`; `herdr
control` reads it from `HERDR_CONTROL_PROTOCOL` and the label from
`HERDR_CONTROL_CLIENT`. Streams that send nothing keep protocol 1: their
`takeover: true` attaches still evict other protocol 1 attaches, they answer
terminal queries first, and they never receive records added since. Protocol 1
and 2 clients share a server; a protocol 1 takeover does not evict protocol 2
attaches. The socket API reference under `docs/next/website` describes the
wire shapes.

## Publishing

Push an immutable `rootshell-vMAJOR.MINOR.PATCH` tag to `kitknox/herdr`. The
Rootshell workflow builds all five platform assets, publishes the installer and
checksums, then promotes `rootshell.json` on the `rootshell-channel` prerelease.
That auxiliary release is the mutable update pointer; versioned releases are
the permanent download locations. Promotion refuses to move backward or replace
an existing version with different metadata. Monitor the complete workflow,
including channel promotion, before announcing a release.

Windows users download `herdr-windows-x86_64.zip` from the tagged release and
keep its extracted directory together, including the app-local ConPTY runtime.
The shell installer above is for macOS and Linux only.
