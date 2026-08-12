#!/usr/bin/env node
'use strict';

// Postinstall: download the prebuilt amux binary for the current platform/arch
// from the GitHub Releases of the amux repo into ./vendor, so the `amux` bin
// wrapper (bin/amux.js) can spawn it. No binary ships in the npm tarball.

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const REPO = 'chasonyu/amux'; // public GitHub repo hosting the Releases

// Map npm platform/arch to the release asset suffix produced by CI.
const TARGETS = {
  darwin: { x64: 'darwin-x64', arm64: 'darwin-arm64' },
  linux:  { x64: 'linux-x64', arm64: 'linux-arm64' },
};

function die(msg) {
  console.error(`[amux postinstall] ${msg}`);
  process.exit(0); // don't fail the whole npm install on unsupported env
}

const suffix = (TARGETS[process.platform] || {})[process.arch];
if (!suffix) {
  die(`no prebuilt binary for ${process.platform}/${process.arch}. Build from source: https://github.com/${REPO}`);
}

const { version } = require('../package.json');
const tag = `v${version}`;
const asset = `amux-${tag}-${suffix}.tar.gz`;
const url = `https://github.com/${REPO}/releases/download/${tag}/${asset}`;

const vendorDir = path.join(__dirname, '..', 'vendor');
const tarball = path.join(vendorDir, asset);
const binPath = path.join(vendorDir, 'amux');

fs.mkdirSync(vendorDir, { recursive: true });

// Use curl instead of node's https: curl transparently honors HTTP(S)_PROXY /
// ALL_PROXY env vars, which is required behind corporate proxies (e.g. Alibaba
// dev boxes) where node's https module would connect directly and hang. curl is
// shipped with macOS and virtually every Linux distro.
console.log(`[amux postinstall] downloading ${asset}`);
try {
  execFileSync('curl', ['-fsSL', '--max-time', '180', '--retry', '2', '-o', tarball, url], { stdio: 'inherit' });
} catch (e) {
  die(`download failed: ${e.message}`);
}

// Extract the tarball (contains a single `amux` binary) into vendor/.
try {
  execFileSync('tar', ['-xzf', tarball, '-C', vendorDir], { stdio: 'inherit' });
} catch (e) {
  die(`extraction failed: ${e.message}`);
}
fs.rmSync(tarball, { force: true });

if (!fs.existsSync(binPath)) {
  die(`binary not found after extraction (expected ${binPath})`);
}
fs.chmodSync(binPath, 0o755);
console.log(`[amux postinstall] ready at ${binPath}`);
