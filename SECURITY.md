# Security policy

## Reporting a vulnerability

Please report security problems privately, not in public issues. Open the
repository's **Security** tab and choose
[**Report a vulnerability**](https://github.com/ShaderCompilation/Esquie/security/advisories/new).

Include the Esquie version, how you installed it, and steps to reproduce. I
will acknowledge the report as soon as I can and keep you updated while it is
being fixed. Esquie is maintained by one person in their spare time, so please
allow reasonable time before disclosing publicly.

Areas of particular interest:

- The update mechanism: release checks, download, signature and checksum
  verification, and in-place replacement of the AppImage.
- Files and launchers Esquie writes: desktop entries, autostart, and per-user
  installation.
- Handling of audio devices and the local single-instance socket.

## Supported versions

Only the latest release receives security fixes. Fixes ship as a new signed
release, which installed copies are offered through the in-app update check.
Versions 0.1.0 and 0.1.1 cannot update themselves; update them
[manually](docs/appimage.md#updating).
