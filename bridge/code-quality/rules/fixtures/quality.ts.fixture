export function unsafe(input: string): unknown {
  // ruleid: omp-quality.javascript.eval
  return eval(input);
}
export function safe(input: string): unknown {
  // ok: omp-quality.javascript.eval
  return JSON.parse(input);
}
