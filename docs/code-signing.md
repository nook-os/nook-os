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

**Signing and notarising are two separate things, and only both together
remove the warning.** A signed but unnotarised app is refused by Gatekeeper on
any Mac that has not seen it before, because the check is "did Apple see this
build", not "is it signed". Our releases are signed today and not yet
notarised, so the gesture below is still needed.

**A trap worth recording**: leaving the notarisation variables in the build
step's `env:` block does not disable notarisation. A missing GitHub secret
becomes an empty string, and an empty string is still a variable that exists —
Tauri read it, tried to notarise, and failed with `The value '' is invalid for
'--issuer'`. They have to be genuinely absent, which means exporting them
through `GITHUB_ENV` only when the key is present.

**Until we buy in**, an unsigned build is still usable, and the site says how:
right-click → **Open** the first time, or

```
xattr -d com.apple.quarantine /Applications/NookOS.app
```

VS Code, which we are otherwise copying here, is signed and notarised —
Microsoft holds a Developer ID. That is the difference between their download
opening cleanly and ours needing a gesture.

## Windows

We already produce both installers — Tauri's WiX bundler emits
`NookOS_<version>_x64_en-US.msi` and its NSIS bundler emits
`NookOS_<version>_x64-setup.exe`. Neither is signed, so SmartScreen shows
"Windows protected your PC" and hides the run button behind **More info**.

### The one thing that changed, and why it matters

Since **June 2023** the CA/Browser Forum requires code-signing private keys to
live in certified hardware — a USB token or an approved HSM. You can no longer
buy a certificate and download a `.pfx`.

That single rule is what makes Windows signing awkward in CI: a USB token
cannot be plugged into a GitHub runner. So the practical choice is not *which
CA* but *which cloud signing service*, because the signing has to happen
somewhere that already holds the key.

### Two grades

| | SmartScreen behaviour |
| --- | --- |
| **OV** (organisation validated) | Warns at first, and the warning fades as downloads accumulate reputation against that certificate |
| **EV** (extended validation) | Trusted immediately, no reputation-building period |

EV costs more and validates harder. For a project nobody has downloaded yet,
OV means the warning persists for a while — which is worth knowing before
paying for OV and finding the problem is still there.

### Options worth comparing

Prices are indicative and change; check current terms rather than trusting
this table.

| Route | Roughly | Notes |
| --- | --- | --- |
| **Azure Trusted Signing** | ~$10/month | Microsoft-run, built for CI, no token to hold. Requires a verified organisation, or an individual who can show ~3 years of verifiable history. Cheapest legitimate path if you qualify. |
| **SignPath Foundation** | free for OSS | Aimed squarely at open-source projects. Has an approval process and constraints on how the build is wired. |
| **Certum Open Source** | ~$30–100/year | Long-standing cheap option for open source; ships a physical token, which is fine locally and awkward in CI. |
| **DigiCert / Sectigo / SSL.com** | ~$200–700/year | The traditional CAs. All now sell a cloud-signing add-on (KeyLocker, eSigner) precisely because of the hardware rule. |

Given NookOS is Apache-2.0 and public, **SignPath Foundation and Azure Trusted
Signing are the two to price out first.**

### How VS Code does it

It does not tell you anything reusable. Microsoft signs VS Code with their own
internal signing infrastructure, using certificates they hold as the operating
system vendor. There is no product to buy that reproduces it — the reason their
download opens without a murmur is that they are Microsoft.

The transferable part is smaller: VS Code ships a **signed** installer, and
that is the whole difference. Nothing about their installer format matters
here; ours is already an MSI and an NSIS setup, which is the same shape.

### Wiring it up when a certificate exists

Tauri signs Windows bundles two ways:

- `bundle.windows.certificateThumbprint` — for a certificate already in the
  machine's certificate store. Works on a self-hosted runner with a token
  attached; not on a hosted runner.
- `bundle.windows.signCommand` — an arbitrary command Tauri calls per artifact.
  This is the hook every cloud signing service plugs into, and the one we will
  use.

The `timestampUrl` is already set in `tauri.conf.json`. Timestamping is not
optional in practice: without it, every signature becomes invalid the day the
certificate expires, including on copies people already downloaded.

## Linux

Nothing is required. `.AppImage` and `.deb` install without a signature. If we
later publish an apt repository, that gets GPG-signed, which is free and
self-issued.

## What this costs, if we decide to do it

| Platform | Requirement | Rough cost |
| --- | --- | --- |
| macOS | Apple Developer Program + notarisation | $99/year |
| Windows | Cloud signing service (hardware key is mandatory) | ~$10/month via Azure, free via SignPath for OSS, or $200–700/year traditional |
| Linux | none | — |

Both would be CI secrets and a signing step in `release.yml`. Tauri supports
both directly, so the change is configuration and credentials rather than new
code.

## Why the browser version sidesteps all of this

The desktop app is the same web build in a native window. Anyone who does not
want to deal with an unsigned bundle can open the control plane in a browser
and get an identical UI. That is the honest recommendation until signing is in
place — and the reason not to treat the desktop app as the primary download.
