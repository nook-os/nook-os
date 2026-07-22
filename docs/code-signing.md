# Code signing for the desktop app

Short answer: **macOS signing requires a paid Apple Developer Program
membership. There is no open-source or free substitute.** Windows is similar,
with one caveat worth chasing. Linux needs nothing.

## macOS

Gatekeeper trusts exactly one thing: a *Developer ID Application* certificate
issued by Apple. That needs an Apple Developer Program membership (currently
$99/year), and the signed app then has to be **notarised** — uploaded to Apple,
scanned, and issued a ticket that gets stapled to the bundle.

There is no community CA, no free tier, and no way to opt out. This is a
deliberate design decision by Apple rather than a gap someone could fill:
Gatekeeper does not consult the system trust store for application signing, so
a certificate from Let's Encrypt or any other public CA is not merely
unsuitable, it is not consulted at all.

`codesign -s -` (ad-hoc signing) works on the machine that produced the bundle
and does nothing for anyone else — a downloaded file carries the
`com.apple.quarantine` attribute, and ad-hoc signatures do not satisfy it.

**Until we buy in**, an unsigned build is still usable, and the site says how:
right-click → **Open** the first time, or

```
xattr -d com.apple.quarantine /Applications/NookOS.app
```

VS Code, which we are otherwise copying here, is signed and notarised —
Microsoft holds a Developer ID. That is the difference between their download
opening cleanly and ours needing a gesture.

## Windows

SmartScreen warns about executables without an Authenticode signature, and the
warning fades as a certificate accumulates reputation. Certificates come from
commercial CAs — roughly $100–400/year, and since June 2023 the private key
must live on a hardware token or in an approved HSM, which complicates signing
from CI.

**Worth investigating rather than asserting:** SignPath offers free code
signing to open-source projects, and other CAs have run similar programmes.
Whether NookOS qualifies, and what it requires of the build, is a question for
whoever picks this up — I have not verified the current terms.

## Linux

Nothing is required. `.AppImage` and `.deb` install without a signature. If we
later publish an apt repository, that gets GPG-signed, which is free and
self-issued.

## What this costs, if we decide to do it

| Platform | Requirement | Rough cost |
| --- | --- | --- |
| macOS | Apple Developer Program + notarisation | $99/year |
| Windows | Authenticode cert (+ hardware token) | $100–400/year, or possibly free for OSS |
| Linux | none | — |

Both would be CI secrets and a signing step in `release.yml`. Tauri supports
both directly, so the change is configuration and credentials rather than new
code.

## Why the browser version sidesteps all of this

The desktop app is the same web build in a native window. Anyone who does not
want to deal with an unsigned bundle can open the control plane in a browser
and get an identical UI. That is the honest recommendation until signing is in
place — and the reason not to treat the desktop app as the primary download.
