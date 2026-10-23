import { nodeConfig } from '@docouno/env/eslint';
import { fileURLToPath } from 'node:url';

export default nodeConfig(fileURLToPath(new URL('.', import.meta.url)));
