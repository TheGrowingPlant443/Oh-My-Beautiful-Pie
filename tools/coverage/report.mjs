import { isAbsolute, relative, resolve, sep } from "node:path";

const GENERATED = ["src-tauri/target/", "node_modules/"];

function addTo(map, key, value) {
  map.set(key, (map.get(key) ?? 0) + value);
}

function newRecord() {
  return { fn: new Set(), fnda: new Map(), da: new Map(), brda: new Map() };
}

function addBranch(record, rest) {
  const parts = rest.split(",");
  const taken = parts.pop();
  const key = parts.join(",");
  const previous = record.brda.get(key) ?? null;
  if (taken === "-") record.brda.set(key, previous);
  else record.brda.set(key, (previous ?? 0) + Number(taken));
}

function addLine(record, tag, rest) {
  switch (tag) {
    case "FN": record.fn.add(rest); break;
    case "FNDA": {
      const comma = rest.indexOf(",");
      addTo(record.fnda, rest.slice(comma + 1), Number(rest.slice(0, comma)));
      break;
    }
    case "DA": {
      const [line, count] = rest.split(",");
      addTo(record.da, Number(line), Number(count));
      break;
    }
    case "BRDA": addBranch(record, rest); break;
    default: break; // TN, LF/LH/FNF/FNH/BRF/BRH are recomputed
  }
}

/** Repository-relative POSIX path of a report path, or null when it is outside the repository or generated. */
function repositoryPath(root, file) {
  const path = relative(root, resolve(root, file));
  if (path === ".." || path.startsWith(`..${sep}`) || isAbsolute(path)) return null;
  const name = path.split(sep).join("/");
  return GENERATED.some(part => name.startsWith(part) || name.includes(`/${part}`)) ? null : name;
}

function render(path, record) {
  const out = ["TN:", `SF:${path}`];
  for (const entry of [...record.fn].sort((a, b) => a.localeCompare(b))) out.push(`FN:${entry}`);
  for (const [name, count] of [...record.fnda].sort((a, b) => a[0].localeCompare(b[0]))) out.push(`FNDA:${count},${name}`);
  out.push(`FNF:${record.fn.size}`, `FNH:${[...record.fnda.values()].filter(count => count > 0).length}`);
  for (const [line, count] of [...record.da].sort((a, b) => a[0] - b[0])) out.push(`DA:${line},${count}`);
  out.push(`LF:${record.da.size}`, `LH:${[...record.da.values()].filter(count => count > 0).length}`);
  const branches = [...record.brda];
  for (const [key, taken] of branches) out.push(`BRDA:${key},${taken ?? "-"}`);
  out.push(`BRF:${branches.length}`, `BRH:${branches.filter(([, taken]) => taken > 0).length}`, "end_of_record");
  return out;
}

function sourceRecord(file, options, records) {
  const path = repositoryPath(options.root, file);
  if (path === null) return null;
  const lines = options.lineCount(path);
  if (lines === null) throw new Error(`coverage report names ${path}, which does not exist`);
  const record = records.get(path) ?? newRecord();
  record.lines = lines;
  records.set(path, record);
  return record;
}
function collectRecords(text, options) {
  const records = new Map();
  let record = null;
  for (const line of text.split(/\r?\n/u)) {
    const colon = line.indexOf(":");
    const tag = colon < 0 ? line : line.slice(0, colon);
    if (tag === "SF") record = sourceRecord(line.slice(colon + 1), options, records);
    else if (tag === "end_of_record") record = null;
    else if (record !== null) addLine(record, tag, line.slice(colon + 1));
  }
  return records;
}
/** Merge actual LCOV hits by original repository-relative source, rejecting impossible line numbers. */
export function normalizeLcov(text, options) {
  const records = collectRecords(text, options);
  const out = [];
  for (const [file, data] of [...records].sort((a, b) => a[0].localeCompare(b[0]))) {
    const beyond = [...data.da.keys()].find(line => !Number.isInteger(line) || line < 1 || line > data.lines);
    if (beyond !== undefined) throw new Error(`coverage report counts line ${beyond} of ${file}, which has ${data.lines} lines`);
    out.push(...render(file, data));
  }
  return out.length ? `${out.join("\n")}\n` : "";
}

/** Counts of covered and coverable lines in a normalized lcov report. */
export function lcovTotals(text) {
  let found = 0;
  let hit = 0;
  for (const line of text.split("\n")) {
    if (line.startsWith("LF:")) found += Number(line.slice(3));
    else if (line.startsWith("LH:")) hit += Number(line.slice(3));
  }
  return { found, hit };
}

