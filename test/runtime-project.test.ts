import assert from 'node:assert/strict';
import test from 'node:test';
import { developmentComponents } from '../tooling/runtime-project.ts';
import type { ComponentState, ProjectState } from '../tooling/types.ts';

function component(toolchain: 'node' | 'rust', service: string): ComponentState {
  return {
    role: 'app',
    toolchain,
    toolingProfile: toolchain === 'node' ? 'node' : 'rust',
    origin: { template: toolchain === 'node' ? 'node-api' : 'rust-api', version: '0.2.0' },
    toolDependencies: {},
    dependencies: [],
    development: { kind: 'compose', owner: 'managed', service },
  };
}

function state(): ProjectState {
  return {
    schemaVersion: 2,
    version: '0.2.0',
    releaseIdentity: '0'.repeat(64),
    generation: '00000000-0000-4000-8000-000000000001',
    packageMode: 'registry',
    managed: {},
    components: {
      'apps/web': component('node', 'frontend'),
      'apps/api': component('rust', 'backend'),
      'apps/unused': component('rust', 'unused'),
    },
  };
}

await test('development admits the transitive normalized service closure, not unrelated apps', () => {
  // Compose normalizes explicit and implicit (links/namespaces/volumes_from)
  // dependencies into this same depends_on shape.
  const model = {
    services: {
      frontend: { depends_on: { helper: { required: true } } },
      helper: { depends_on: { backend: { required: true } } },
      backend: {},
      unused: {},
    },
  };
  assert.deepEqual(developmentComponents(state(), model, 'frontend'), ['apps/api', 'apps/web']);
  assert.deepEqual(developmentComponents(state(), model, 'backend'), ['apps/api']);
  assert.throws(() => developmentComponents(state(), model, 'absent'), /selected Compose service/u);
});

await test('disabled optional profiles stay outside admission; required missing services fail', () => {
  const model = {
    services: {
      frontend: { profiles: ['client'], depends_on: { backend: { required: false } } },
      backend: { profiles: ['server'] },
      unused: {},
    },
  };
  assert.deepEqual(developmentComponents(state(), model, 'frontend'), ['apps/web']);
  model.services.frontend.depends_on.backend.required = true;
  assert.throws(() => developmentComponents(state(), model, 'frontend'), /not active/u);
  assert.throws(
    () =>
      developmentComponents(
        state(),
        { services: { frontend: { depends_on: { missing: {} } } } },
        'frontend',
      ),
    /Compose service missing/u,
  );
});
