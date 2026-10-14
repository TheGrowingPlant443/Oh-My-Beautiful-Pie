import type { DiffFile } from "./api";

export type TreeNode =
  | { kind: "folder"; key: string; name: string; depth: number; count: number; children: TreeNode[] }
  | { kind: "file"; name: string; depth: number; file: DiffFile };

type FolderNode = Extract<TreeNode, { kind: "folder" }>;

/** Folder tree for slash-separated paths. `prefix` keeps folder keys of different lists (groups) distinct. */
export function buildTree(files: DiffFile[], prefix: string): TreeNode[] {
  const roots: TreeNode[] = [];
  const folders = new Map<string, FolderNode>();
  for (const file of files) {
    const parts = file.path.split("/");
    let siblings = roots;
    let path = "";
    for (let depth = 0; depth < parts.length - 1; depth++) {
      path = path ? `${path}/${parts[depth]}` : parts[depth];
      const key = `${prefix}/${path}`;
      let folder = folders.get(key);
      if (!folder) {
        folder = { kind: "folder", key, name: parts[depth], depth, count: 0, children: [] };
        folders.set(key, folder);
        siblings.push(folder);
      }
      folder.count++;
      siblings = folder.children;
    }
    siblings.push({ kind: "file", name: parts[parts.length - 1], depth: parts.length - 1, file });
  }
  const order = (nodes: TreeNode[]): void => {
    nodes.sort((a, b) => (a.kind === b.kind ? (a.name < b.name ? -1 : a.name > b.name ? 1 : 0) : a.kind === "folder" ? -1 : 1));
    for (const node of nodes) if (node.kind === "folder") order(node.children);
  };
  order(roots);
  return roots;
}

/** Depth-first rows, leaving out everything below a collapsed folder. */
export function visibleRows(nodes: TreeNode[], collapsed: ReadonlySet<string>): TreeNode[] {
  const rows: TreeNode[] = [];
  const walk = (list: TreeNode[]): void => {
    for (const node of list) {
      rows.push(node);
      if (node.kind === "folder" && !collapsed.has(node.key)) walk(node.children);
    }
  };
  walk(nodes);
  return rows;
}
