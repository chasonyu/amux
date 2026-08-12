#!/usr/bin/env node
'use strict';

const { spawnSync } = require('child_process');
const { join } = require('path');

const bin = join(__dirname, '..', 'vendor', 'amux');
const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });

// Preserve signal exit when the child died from a signal.
const sig = result.signal;
if (sig) {
  process.kill(process.pid, sig);
}
process.exit(result.status == null ? 1 : result.status);
