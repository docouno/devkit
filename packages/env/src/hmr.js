import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';

const opening = 'const transport = normalizeModuleRunnerTransport((() => {';
const closing = '})());';
const acceptedVersions = new Set(['8.3.5', '8.3.8']);
const fragmentSha = '1b4cea2bb341695cc81fe550e6425ab04e2db736996379b5f743808a108c7897';

export function hmrRecovery() {
  let warned = false;
  const changesRouting = (options) =>
    options &&
    typeof options === 'object' &&
    ['host', 'protocol', 'port', 'clientPort', 'server'].some((key) => options[key] !== undefined);
  return (code, id, warn, server) => {
    if (
      typeof id !== 'string' ||
      id.includes('?') ||
      !id.replaceAll('\\', '/').endsWith('/vite/dist/client/client.mjs')
    ) {
      return null;
    }
    const hmr = server?.hmr;
    const ws = server?.ws;
    // Vite resolves deprecated HMR routing options into server.ws.
    if (hmr === false || ws === false || changesRouting(ws)) {
      return null;
    }
    const complain = () => {
      if (!warned) {
        warned = true;
        warn(
          'Devkit HMR recovery skipped: unsupported Vite client/version; use a tested Vite 8.3.5 or 8.3.8 client or configure server.hmr explicitly.',
        );
      }
      return null;
    };
    let version;
    try {
      version = JSON.parse(
        fs.readFileSync(path.resolve(path.dirname(id), '../../package.json'), 'utf8'),
      ).version;
    } catch {
      return complain();
    }
    const start = code.indexOf(opening);
    const end = code.indexOf(closing, start);
    if (
      !acceptedVersions.has(version) ||
      start < 0 ||
      code.indexOf(opening, start + 1) !== -1 ||
      end < start
    ) {
      return complain();
    }
    const finish = end + closing.length;
    const original = code.slice(start, finish);
    if (createHash('sha256').update(original).digest('hex') !== fragmentSha) {
      return complain();
    }
    const nativeBody = original.slice(opening.length, -closing.length);
    const replacement = `const transport = normalizeModuleRunnerTransport((() => {
    if (typeof document === "undefined") return (() => { ${nativeBody} })();
    let phase = "awaiting-connected";
    let recoveryPromise;
    const key = "devkit:hmr-recovery:" + socketProtocol + "://" + socketHost;
    const wsTransport = createWebSocketModuleRunnerTransport({
      createConnection: () => new WebSocket(socketProtocol + "://" + socketHost + "?token=" + wsToken, "vite-hmr"),
      pingInterval: hmrTimeout
    });
    function recoverInitial() {
      if (recoveryPromise) return recoveryPromise;
      recoveryPromise = Promise.resolve().then(async () => {
        if (phase === "connected") return;
        phase = "recovering";
        let allowed = false;
        try {
          const prev = JSON.parse(sessionStorage.getItem(key) || "null");
          if (prev === null || (prev.version === 1 && typeof prev.token === "string" && prev.spent === true && prev.token !== wsToken)) {
            sessionStorage.setItem(key, JSON.stringify({version: 1, token: wsToken, spent: true}));
            allowed = true;
          }
        } catch {}
        if (!allowed) {
          phase = "stopped";
          console.error("[devkit] HMR recovery stopped: reload budget exhausted or unavailable");
          return;
        }
        await waitForSuccessfulPing(socketProtocol + "://" + socketHost);
        if (!willUnload && phase !== "connected") location.reload();
      }).catch(error => {
        phase = "stopped";
        console.error("[devkit] HMR recovery stopped: " + error);
      });
      return recoveryPromise;
    }
    return {
      async connect(handlers) {
        const wrapped = {...handlers, onMessage(payload) {
          if (payload.type === "connected") {
            phase = "connected";
            try {
              sessionStorage.removeItem(key);
            } catch {}
          }
          if (phase !== "connected" && payload.type === "custom" && payload.event === "vite:ws:disconnect") {
            void recoverInitial();
            return;
          }
          handlers.onMessage(payload);
        }};
        try { await wsTransport.connect(wrapped); }
        catch (error) {
          if (phase !== "connected") { await recoverInitial(); return; }
          throw error;
        }
      },
      async disconnect() { await wsTransport.disconnect(); },
      send(data) { wsTransport.send(data); }
    };
  })());`;
    return { code: code.slice(0, start) + replacement + code.slice(finish), map: null };
  };
}
