# Contributing to Esquie

Esquie is a maintainer-led Linux microphone noise-removal app. The most useful
contributions right now are testing, reproducible bug reports, compatibility
reports, and thoughtful feature requests. Use this repository's Issues tab.
Search existing issues before opening a new one.

## Report a bug or compatibility result

Include the Esquie version and installation type, distro, desktop and Wayland/X11
session, PipeWire/WirePlumber versions, microphone type, steps to reproduce,
and expected versus observed behavior. Say whether the problem reproduces with
noise removal disabled. For testing reports, describe exactly what you tried;
a successful launch does not establish speech quality or suspend/resume support.

You can export a plain-text report from Settings → Diagnostics. Export is local;
Esquie does not upload it. Review and redact it before attaching it to a public
issue: logs may include local paths, device names, and other identifying details.
Recordings are optional; only share audio you are comfortable publishing.
See [troubleshooting](docs/troubleshooting.md).

## Suggest a feature

Describe the problem, who encounters it, your current workaround, and the
behavior you would like. Requests are discussed against the app's direction
and maintenance cost; a request is not a commitment to implement it.

## Before writing code

Open an issue and obtain explicit maintainer approval for the proposed scope
before starting any code contribution, including fixes and refactors. This helps
avoid spending time on work that does not fit the app's vision. A PR submitted
without prior approval may still be merged, but also may just be declined for that reason.

Approval agrees on a direction and scope, not guaranteed acceptance of the
implementation. Keep the change focused, discuss scope changes in the issue,
and link the approved issue in the PR. Explain the resulting behavior and the
checks you ran, including any checks you could not run.

## Build and check

Follow [building Esquie](docs/building.md) for dependencies and the pinned toolchain.
Use the committed lockfile:

```sh
cargo fmt --all -- --check
cargo build --release --locked
cargo test --release --locked
```

`bash ci/check.sh` runs the wider checks with the documented D-Bus, audio, and
virtual-display tools. Keep generated output under `target/`. Do not commit
personal notes, exported diagnostics, recordings, or build artifacts.
Include in PR description the necessary information when applicable.

## License

Contributions intentionally submitted for inclusion are provided under
GPL-3.0-only, the project's license. You retain copyright in your contributions.
No contributor license agreement or copyright assignment is required. Third-party
material must retain its notices and have a compatible license.

This contribution process governs acceptance into this repository. It does not
add restrictions to the rights granted by the GPL to use, modify, or fork Esquie.
