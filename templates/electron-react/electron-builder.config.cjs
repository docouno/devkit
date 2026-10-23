const { dirname, join } = require('node:path');
module.exports = {
  appId: 'org.example.desktop',
  productName: 'Desktop',
  directories: { output: 'release' },
  files: ['dist/**', 'package.json'],
  asar: true,
  npmRebuild: false,
  electronDist: join(dirname(require.resolve('electron/package.json')), 'dist'),
  linux: { executableName: 'app', target: ['dir'] },
};
