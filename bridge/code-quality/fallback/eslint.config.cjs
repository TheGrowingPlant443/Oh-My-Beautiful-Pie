const { createRequire } = require('node:module');
const imageRequire = createRequire('/node-deps/package.json');
const js = imageRequire('@eslint/js');
const tsPlugin = imageRequire('@typescript-eslint/eslint-plugin');
const tsParser = imageRequire('@typescript-eslint/parser');
module.exports = [
  { ...js.configs.recommended, files: ['**/*.{js,cjs,mjs,jsx}'], languageOptions: { globals: { console: 'readonly', process: 'readonly', Buffer: 'readonly', module: 'readonly', require: 'readonly', exports: 'readonly', __dirname: 'readonly', window: 'readonly', document: 'readonly', fetch: 'readonly' } } },
  { files: ['**/*.{ts,tsx}'], languageOptions: { parser: tsParser, parserOptions: { ecmaFeatures: { jsx: true } } }, plugins: { '@typescript-eslint': tsPlugin }, rules: { ...tsPlugin.configs.recommended.rules, '@typescript-eslint/no-explicit-any': 'off', '@typescript-eslint/explicit-function-return-type': 'off', '@typescript-eslint/consistent-type-definitions': 'off' } }
];
