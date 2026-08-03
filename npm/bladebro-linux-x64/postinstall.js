'use strict';
const { chmodSync } = require('fs');
const { join } = require('path');
try {
  chmodSync(join(__dirname, 'bladebro'), 0o755);
} catch (e) {}
