function unsafe(input) {
  // ruleid: omp-quality.javascript.eval
  eval(input);
  // ruleid: omp-quality.javascript.function-constructor
  const execute = new Function('data', input);
  // ruleid: omp-quality.javascript.tls-verification-disabled
  const tls = { rejectUnauthorized: false };
  return [execute, tls];
}
function safe(input) {
  // ok: omp-quality.javascript.eval
  const data = JSON.parse(input);
  // ok: omp-quality.javascript.tls-verification-disabled
  const tls = { rejectUnauthorized: true };
  // ok: omp-quality.javascript.tls-verification-disabled
  const defaults = {};
  return [data, tls, defaults];
}
