// Monaco is bundled locally (no CDN or remote loader). Only the editor worker is wired: the diff is computed
// there; language servers/IntelliSense are intentionally not loaded, so languages get Monarch highlighting only.
import * as monaco from "monaco-editor/editor/editor.api";
import "monaco-editor/basic-languages/monaco.contribution";
import EditorWorker from "monaco-editor/editor/editor.worker?worker";

globalThis.MonacoEnvironment = { getWorker: () => new EditorWorker() };

export { monaco };

export function languageFor(path: string): string {
  const name = path.slice(path.lastIndexOf("/") + 1).toLowerCase();
  const dot = name.lastIndexOf(".");
  const extension = dot > 0 ? name.slice(dot) : "";
  const language = monaco.languages.getLanguages().find(candidate => candidate.filenames?.includes(name) || (extension !== "" && candidate.extensions?.includes(extension)));
  return language?.id ?? "plaintext";
}
