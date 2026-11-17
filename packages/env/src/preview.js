import fs from 'node:fs';
import { createHash } from 'node:crypto';
import { isIP } from 'node:net';
import { domainToUnicode } from 'node:url';

const slots = ['/run/devkit/preview-hosts.json', '/devkit/preview-hosts.json'];
const maxBytes = 32 * 1024 * 1024;
const fail = (message) => {
  throw new Error(`Devkit preview: ${message}`);
};
const plain = (v) => v !== null && typeof v === 'object' && !Array.isArray(v);

export function exactHost(raw) {
  if (typeof raw !== 'string' || raw.length > 254 || !raw) {
    fail('invalid exact host');
  }
  const host = raw.startsWith('[') && raw.endsWith(']') ? raw.slice(1, -1) : raw;
  const ip = isIP(host);
  const lower = host.toLowerCase();
  const name = lower.endsWith('.') ? lower.slice(0, -1) : lower;
  if (
    !ip &&
    (name.length > 253 ||
      !name.split('.').every((label) => /^[a-z\d](?:[a-z\d-]{0,61}[a-z\d])?$/u.test(label)))
  ) {
    fail('invalid exact hostname');
  }
  for (const label of !ip ? name.split('.') : []) {
    if (label.startsWith('xn--')) {
      const decoded = domainToUnicode(label);
      if (!decoded || decoded === label) {
        fail('invalid IDNA hostname');
      }
    }
  }
  let normalized;
  try {
    normalized = new URL(`http://${ip === 6 ? `[${host}]` : host}/`).hostname;
  } catch {
    fail('invalid browser hostname');
  }
  normalized = normalized.startsWith('[') ? normalized.slice(1, -1) : normalized;
  if (normalized === '0.0.0.0' || normalized === '::') {
    fail('wildcard address');
  }
  return normalized;
}
function signature(stat) {
  return [stat.dev, stat.ino, stat.size, stat.mtimeNs, stat.ctimeNs].join(':');
}
function ancestors(file) {
  const result = [];
  const parts = file.split('/').filter(Boolean);
  let current = '';
  for (const part of parts.slice(0, -1)) {
    current += `/${part}`;
    const stat = fs.lstatSync(current, { bigint: true });
    if (!stat.isDirectory() || stat.isSymbolicLink()) {
      fail('unsafe metadata ancestry');
    }
    result.push([current, signature(stat)]);
  }
  return result;
}
function readHosts(file, sha, service) {
  if (!slots.includes(file) || !/^[a-f\d]{64}$/u.test(sha ?? '') || !service) {
    fail('invalid FILE/SHA256/SERVICE metadata contract');
  }
  const parents = ancestors(file);
  const descriptor = fs.openSync(
    file,
    fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK,
  );
  let bytes;
  try {
    const before = fs.fstatSync(descriptor, { bigint: true });
    if (!before.isFile() || before.size > BigInt(maxBytes)) {
      fail('metadata must be a bounded regular file');
    }
    bytes = Buffer.alloc(Number(before.size));
    let count = 0;
    while (count < bytes.length) {
      const read = fs.readSync(descriptor, bytes, count, bytes.length - count, count);
      if (!read) {
        fail('truncated metadata');
      }
      count += read;
    }
    const after = fs.fstatSync(descriptor, { bigint: true });
    const named = fs.lstatSync(file, { bigint: true });
    if (
      signature(before) !== signature(after) ||
      signature(after) !== signature(named) ||
      !named.isFile() ||
      named.isSymbolicLink()
    ) {
      fail('metadata changed during read');
    }
    for (const [parent, identity] of parents) {
      const stat = fs.lstatSync(parent, { bigint: true });
      if (!stat.isDirectory() || stat.isSymbolicLink() || signature(stat) !== identity) {
        fail('metadata ancestry changed');
      }
    }
  } finally {
    fs.closeSync(descriptor);
  }
  if (createHash('sha256').update(bytes).digest('hex') !== sha) {
    fail('metadata digest mismatch');
  }
  const document = JSON.parse(bytes.toString('utf8'));
  if (
    !plain(document) ||
    Object.keys(document).sort().join(',') !== 'hosts,schemaVersion,service' ||
    document.schemaVersion !== 1 ||
    document.service !== service ||
    !Array.isArray(document.hosts) ||
    document.hosts.length > 65535
  ) {
    fail('invalid metadata schema/service/count');
  }
  if (
    Buffer.byteLength(JSON.stringify({ schemaVersion: 1, service: document.service, hosts: [] })) >
    65536
  ) {
    fail('metadata header exceeds budget');
  }
  let previous = '';
  for (const host of document.hosts) {
    if (exactHost(host) !== host || host <= previous) {
      fail('metadata hosts must be canonical, sorted and unique');
    }
    previous = host;
  }
  return document.hosts;
}
function csv(raw, suffixes) {
  if (!raw) {
    return [];
  }
  if (Buffer.byteLength(raw) > 65536) {
    fail('manual hosts exceed 64 KiB');
  }
  return raw.split(',').map((value) => {
    const host = value.trim();
    return suffixes && host.startsWith('.') ? `.${exactHost(host.slice(1))}` : exactHost(host);
  });
}
export function previewHosts(environment = process.env) {
  const file = environment.DEVKIT_PREVIEW_HOSTS_FILE;
  const hosts = file
    ? readHosts(file, environment.DEVKIT_PREVIEW_HOSTS_SHA256, environment.DEVKIT_SERVICE)
    : [];
  if (!file && environment.DEVKIT_PREVIEW_HOSTS_SHA256) {
    fail('SHA256 requires FILE');
  }
  if (environment.DEVKIT_PREVIEW_URL) {
    const url = new URL(environment.DEVKIT_PREVIEW_URL);
    if (
      !['http:', 'https:'].includes(url.protocol) ||
      url.username ||
      url.password ||
      url.port === '0'
    ) {
      fail('invalid concrete preview URL');
    }
    const host = exactHost(url.hostname);
    if (file && !hosts.includes(host)) {
      fail('primary URL is outside metadata host set');
    }
    hosts.push(host);
  }
  hosts.push(
    ...(!file ? csv(environment.DEVKIT_PREVIEW_HOSTS, false) : []),
    ...csv(environment.DEVKIT_ALLOWED_HOSTS, true),
    ...csv(environment.VITE_ALLOWED_HOSTS, true),
  );
  return [...new Set(hosts)];
}
export function hostPolicy(allowed) {
  if (allowed === true) {
    return () => true;
  }
  const exact = new Set(allowed);
  const suffixes = allowed.filter((host) => host.startsWith('.'));
  return (authority) => {
    if (typeof authority !== 'string' || /[,\s/@\\?#]/u.test(authority)) {
      return false;
    }
    let host;
    try {
      const url = new URL(`http://${authority}`);
      if (url.username || url.password || url.pathname !== '/') {
        return false;
      }
      // Hostnames in explicit Vite configuration retain Vite's literal matching.
      host = authority.startsWith('[') ? url.hostname.slice(1, -1) : authority.split(':', 1)[0];
    } catch {
      return false;
    }
    return (
      Boolean(isIP(host)) ||
      host === 'localhost' ||
      host.endsWith('.localhost') ||
      exact.has(host) ||
      suffixes.some((suffix) => host === suffix.slice(1) || host.endsWith(suffix))
    );
  };
}
export function configurePreviewHosts(server) {
  const hosts = previewHosts();
  if (server.allowedHosts !== true && hosts.length > 0) {
    server.allowedHosts = [...new Set([...(server.allowedHosts ?? []), ...hosts])];
  }
}
export function configureApiProxy(server, allowsHost) {
  const api = server.proxy?.['/api'];
  const target =
    typeof api === 'string' ? api : api === undefined ? process.env.DEVKIT_API_TARGET : undefined;
  if (target) {
    server.proxy ??= {};
    server.proxy['/api'] = {
      target,
      changeOrigin: true,
      ws: true,
      configure(instance) {
        const forward = instance.ws;
        instance.ws = function guardedUpgrade(request, socket, ...rest) {
          if (!allowsHost(request.headers.host)) {
            socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
            return undefined;
          }
          return Reflect.apply(forward, this, [request, socket, ...rest]);
        };
      },
    };
  }
}
