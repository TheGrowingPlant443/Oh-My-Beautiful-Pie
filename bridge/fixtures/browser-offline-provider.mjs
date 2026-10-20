// Browser integration uses native eval only. A session still needs a selected model;
// this in-process provider fails closed if a fixture accidentally attempts inference.
export default function browserOfflineProvider(pi) {
  pi.registerProvider('browser-offline', {
    baseUrl: 'mock://browser-fixture',
    apiKey: 'offline-fixture-key',
    api: 'browser-offline',
    streamSimple() { throw new Error('Browser fixture must not call a model'); },
    models: [{
      id: 'fixture', name: 'Browser fixture', reasoning: false, input: ['text'],
      cost: {input: 0, output: 0, cacheRead: 0, cacheWrite: 0},
      contextWindow: 32768, maxTokens: 1024,
    }],
  });
}
