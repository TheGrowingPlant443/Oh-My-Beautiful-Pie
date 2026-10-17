import { api } from "./api";
import { QualityClient } from "./code-quality";

// No localStorage configuration: all mounted surfaces subscribe to this one backend snapshot.
export const codeQualityClient = new QualityClient({
  read: api.getCodeQualityState,
  setTimer: (fn, ms) => window.setTimeout(fn, ms),
  clearTimer: handle => window.clearTimeout(handle as number),
});
