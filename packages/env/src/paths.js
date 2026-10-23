import path from 'node:path';

export const within = (root, target) => {
  const relative = path.relative(root, target);
  return (
    relative === '' ||
    (!relative.startsWith(`..${path.sep}`) && relative !== '..' && !path.isAbsolute(relative))
  );
};

export function relativeList(value, fallback, field) {
  const list = value === undefined ? fallback : value;
  if (!Array.isArray(list) || list.length === 0) {
    throw new Error(`${field} must be a nonempty array.`);
  }
  const normalized = list.map((entry) => {
    if (
      typeof entry !== 'string' ||
      entry !== entry.trim() ||
      !entry ||
      [...entry].some((character) => {
        const code = character.charCodeAt(0);
        return code < 32 || code === 127 || '\\*?[]{}'.includes(character);
      }) ||
      path.isAbsolute(entry) ||
      entry.split('/').includes('..') ||
      path.normalize(entry) === '.'
    ) {
      throw new Error(
        `${field} entries must be safe relative paths without globs or parent traversal.`,
      );
    }
    return path.normalize(entry);
  });
  if (new Set(normalized).size !== normalized.length) {
    throw new Error(`${field} contains duplicate paths.`);
  }
  return normalized;
}
