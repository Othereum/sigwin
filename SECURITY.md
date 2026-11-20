# Security policy

## Reporting a vulnerability

Please report security issues privately through
[GitHub's private vulnerability reporting](https://github.com/Othereum/sigwin/security/advisories/new),
not in a public issue.

Include what an attacker can do, the steps to reproduce it, and the Sigwin version
(Settings → About). Leave out real message contents, phone numbers and keys; a description
of their shape is enough.

Sigwin is maintained by one person in their spare time. You'll get a reply as soon as
possible, and a fix is released as a new version, which installed apps pick up through
their update check.

## Supported versions

Only the latest release is supported. Sigwin updates itself, so fixes are not backported.

## Scope

In scope: Sigwin's own code, such as how it stores account data on disk, what it logs,
the update check and its signature verification, and handling of received messages and
attachments.

Out of scope: the Signal protocol, Signal's servers and the official apps. Report those to
Signal at security@signal.org. Issues in libraries
Sigwin builds on (presage, libsignal-service-rs, Slint) are welcome here too if they affect
Sigwin, but are best reported upstream as well.
