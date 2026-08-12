#!/usr/bin/env node
'use strict';

// Postinstall: download the prebuilt amux binary for the current platform/arch
// from the GitHub Releases of the amux repo into ./vendor, so the `amux` bin
// wrapper (bin/amux.js) can spawn it. No binary ships in the npm tarball.

const fs = require('fs');
const path = require('path');
const https = require('https');
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

function download(url, dest, redirectsLeft) {
  return new Promise((resolve, reject) => {
    const file = fs.createWriteStream(dest);
    const req = https.get(url, (res) => {
      // Follow GitHub's 302 → objects.githubusercontent.com redirect chain.
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location && redirectsLeft > 0) {
        file.close();
        fs.rmSync(dest, { force: true });
        return resolve(download(res.headers.location, dest, redirectsLeft - 1));
      }
      if (res.statusCode !== 200) {
        file.close();
        fs.rmSync(dest, { force: true });
        return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
      }
      res.pipe(file);
      file.on('finish', () => file.close(resolve));
    });
    req.on('error', reject);
    req.setTimeout(120000, () => req.destroy(new Error('download timeout')));
  });
}

(async () => {
  console.log(`[amux postinstall] downloading ${asset}`);
  try {
    await download(url, tarball, 5);
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
})();
