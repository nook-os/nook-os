#!/usr/bin/env node
// Proof that the image's browser WORKS, not an assertion that it is installed
// (MAIN-595 AC-2). It runs once during the image build — so an image whose
// Chromium cannot start never ships — and stays on PATH afterwards, so an
// operator can re-run it against a live container:
//
//   docker compose exec operator-node nook-browser-check
//
// Three things are checked, and each is a way the image has actually been
// wrong before it was checked: the Playwright version matches the build ARG
// (a floating install is not reproducible), only Chromium is on disk (AC-4 —
// `--with-deps chromium` is one typo away from downloading three browsers),
// and a real navigation renders in a real headless process.
'use strict';

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

// Playwright is a GLOBAL npm install, and node does not resolve globals from a
// script living elsewhere. Ask npm where its root is rather than hardcoding
// /usr/lib/node_modules, which differs between the Debian and NodeSource
// layouts the two images are built on.
const globalRoot = execFileSync('npm', ['root', '-g'], { encoding: 'utf8' }).trim();
const playwrightDir = path.join(globalRoot, 'playwright');
const { chromium } = require(playwrightDir);
const installed = require(path.join(playwrightDir, 'package.json')).version;

const fail = (msg) => {
  console.error(`FATAL: ${msg}`);
  process.exit(1);
};

const pinned = process.env.NOOK_PLAYWRIGHT_VERSION;
if (!pinned) fail('NOOK_PLAYWRIGHT_VERSION is not set — nothing to check the install against');
if (installed !== pinned) fail(`playwright ${installed} is installed, but the image pins ${pinned}`);

const browsersPath = process.env.PLAYWRIGHT_BROWSERS_PATH;
if (!browsersPath) fail('PLAYWRIGHT_BROWSERS_PATH is not set — the browsers are wherever HOME points today');
const unwanted = fs
  .readdirSync(browsersPath)
  .filter((entry) => /^(firefox|webkit)[-_]/.test(entry));
if (unwanted.length) fail(`AC-4: only chromium may be installed, found ${unwanted.join(', ')}`);

(async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    await page.goto('data:text/html,<title>nook browser check</title><p id="probe">launched</p>');
    const title = await page.title();
    const probe = await page.textContent('#probe');
    if (title !== 'nook browser check' || probe !== 'launched') {
      fail(`the page loaded but rendered nothing recognisable (title=${title}, probe=${probe})`);
    }
    console.log(`browser check OK — playwright ${installed}, ${browser.version()}, browsers in ${browsersPath}`);
  } finally {
    await browser.close();
  }
})().catch((err) => fail(`chromium did not launch and load a page: ${err.message}`));
