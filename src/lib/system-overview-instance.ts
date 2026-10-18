import { api } from "./api";
import { createStructureStore } from "./structure-overview";
import { createSystemOverviewStore, effectiveIndex } from "./system-overview";

/**
 * Parent-lived stores: the System overview page, its generation tickets and the structure area survive the page being unmounted
 * (for example while the user follows The Architect). App attaches the navigation hooks; nothing here talks to a model.
 */
export const systemOverview = createSystemOverviewStore({ api });
export const structureOverview = createStructureStore();
systemOverview.attachStructureSink(structureOverview);
structureOverview.connect(systemOverview.structureHost(), api);

// The structure area follows the shown folder and index snapshot; it reads saved results only.
let shown = "";
systemOverview.subscribe(state => {
  const index = effectiveIndex(state);
  const folder = index?.folder ?? state.folder;
  const snapshot = index?.snapshotId ?? null;
  const key = JSON.stringify([folder, snapshot]);
  if (key === shown) return;
  shown = key;
  structureOverview.setFolder(folder, snapshot);
});

// Choosing another target Markdown ends the automatic return of generations made for the previous one.
let selectedSource: string | null | undefined;
structureOverview.subscribe(state => {
  if (state.selectedSourceId === selectedSource) return;
  selectedSource = state.selectedSourceId;
  systemOverview.noteSourceSelection(selectedSource);
});
