const { app, BrowserWindow } = require('electron');
app.setPath('userData', '/tmp/devkit-browser-test');
app.whenReady().then(async () => {
  const window = new BrowserWindow({
    width: 1000,
    height: 760,
    webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false },
  });
  await window.loadURL(process.env.PROBE_BROWSER_URL);
});
app.on('window-all-closed', () => app.quit());
