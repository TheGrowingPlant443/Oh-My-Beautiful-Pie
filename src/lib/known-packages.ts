import type { DepNode } from "./dependency-graph";

/**
 * Static, offline, language-independent knowledge used by the two dependency-tree filters.
 * It is deliberately separate from the Architect's per-project "known standard" judgement.
 */
export interface KnownOwner {
  id: string; name: string; line: string;
  /** Lower-case GitHub organisation/user logins. */
  githubLogins: string[];
  /** Lower-case npm scopes including the leading `@`. */
  scopes: string[];
  /** Lower-case Maven group prefixes. */
  groupPrefixes: string[];
  /** Lower-case exact package names. */
  names: string[];
  namePrefixes: string[];
}

const owner = (id: string, name: string, line: string, parts: Partial<Omit<KnownOwner, "id" | "name" | "line">>): KnownOwner =>
  ({ id, name, line, githubLogins: [], scopes: [], groupPrefixes: [], names: [], namePrefixes: [], ...parts });

export const KNOWN_OWNERS: readonly KnownOwner[] = [
  owner("google", "Google", "Google — technology company behind Search, Android, Chrome, Angular and Google Cloud.", {
    githubLogins: ["google", "googleapis", "googlechrome", "googlecloudplatform", "google-github-actions", "angular", "material-components", "flutter", "tensorflow", "firebase"],
    scopes: ["@angular", "@google-cloud", "@googleapis", "@googlemaps", "@firebase", "@material", "@tensorflow"],
    groupPrefixes: ["com.google.", "androidx.", "com.android."],
  }),
  owner("microsoft", "Microsoft", "Microsoft — software company behind Windows, Azure, Visual Studio Code, TypeScript and GitHub.", {
    githubLogins: ["microsoft", "azure", "dotnet", "github", "powershell"],
    scopes: ["@microsoft", "@azure", "@vscode", "@fluentui", "@playwright"],
    groupPrefixes: ["com.microsoft.", "com.azure."], names: ["typescript"],
  }),
  owner("amazon", "Amazon", "Amazon — e-commerce and cloud company that runs Amazon Web Services (AWS).", {
    githubLogins: ["aws", "amazon", "awslabs", "aws-amplify", "amzn", "aws-powertools"],
    scopes: ["@aws-sdk", "@aws-cdk", "@aws-amplify", "@smithy", "@aws-lambda-powertools"],
    groupPrefixes: ["software.amazon.", "com.amazonaws."], names: ["boto3", "botocore", "awscli"],
  }),
  owner("meta", "Meta", "Meta (Facebook) — social media company behind Facebook, Instagram and React.", {
    githubLogins: ["facebook", "facebookincubator", "facebookresearch", "meta"],
    scopes: ["@react-native"], names: ["react", "react-dom", "react-native"],
  }),
  owner("nvidia", "NVIDIA", "NVIDIA — maker of GPUs and AI computing platforms such as CUDA.", {
    githubLogins: ["nvidia", "nvlabs", "rapidsai"], scopes: ["@nvidia"], namePrefixes: ["nvidia-"],
  }),
  owner("netflix", "Netflix", "Netflix — streaming entertainment company.", {
    githubLogins: ["netflix", "netflix-skunkworks"], scopes: ["@netflix"], groupPrefixes: ["com.netflix."],
  }),
  owner("apple", "Apple", "Apple — consumer electronics and software company (iPhone, Mac, Swift).", {
    githubLogins: ["apple", "swiftlang"], scopes: ["@apple"],
  }),
  owner("ibm", "IBM", "IBM — enterprise technology and consulting company.", {
    githubLogins: ["ibm"], scopes: ["@ibm", "@carbon"], groupPrefixes: ["com.ibm."],
  }),
  owner("oracle", "Oracle", "Oracle — database and enterprise software company behind Java and MySQL.", {
    githubLogins: ["oracle", "graalvm", "mysql"], groupPrefixes: ["com.oracle.", "org.graalvm."],
  }),
  owner("adobe", "Adobe", "Adobe — software company behind Photoshop, Acrobat and Creative Cloud.", {
    githubLogins: ["adobe"], scopes: ["@adobe"],
  }),
  owner("salesforce", "Salesforce", "Salesforce — cloud CRM software company.", {
    githubLogins: ["salesforce"], scopes: ["@salesforce", "@lwc"],
  }),
  owner("intel", "Intel", "Intel — semiconductor and computing hardware company.", {
    githubLogins: ["intel"], scopes: ["@intel"],
  }),
  owner("cisco", "Cisco", "Cisco — networking and security hardware and software company.", {
    githubLogins: ["cisco"], scopes: ["@cisco"],
  }),
  owner("uber", "Uber", "Uber — ride-hailing and delivery technology company.", {
    githubLogins: ["uber"], scopes: ["@uber"],
  }),
  owner("airbnb", "Airbnb", "Airbnb — online home-rental marketplace.", {
    githubLogins: ["airbnb"], scopes: ["@airbnb"],
  }),
  owner("shopify", "Shopify", "Shopify — e-commerce platform company.", {
    githubLogins: ["shopify"], scopes: ["@shopify"],
  }),
  owner("stripe", "Stripe", "Stripe — online payments company.", {
    githubLogins: ["stripe"], scopes: ["@stripe"],
  }),
  owner("atlassian", "Atlassian", "Atlassian — software company behind Jira, Confluence and Bitbucket.", {
    githubLogins: ["atlassian", "atlassian-labs"], scopes: ["@atlaskit", "@atlassian"], groupPrefixes: ["com.atlassian."],
  }),
  owner("sap", "SAP", "SAP — enterprise business software company.", {
    githubLogins: ["sap", "sap-samples"], scopes: ["@sap", "@ui5"], groupPrefixes: ["com.sap."],
  }),
  owner("redhat", "Red Hat", "Red Hat — enterprise open-source company (Linux, OpenShift, Quarkus).", {
    githubLogins: ["redhat-developer", "openshift", "quarkusio", "keycloak"], groupPrefixes: ["org.jboss.", "io.quarkus."],
  }),
  owner("docker", "Docker", "Docker — container platform company.", {
    githubLogins: ["docker", "moby"], scopes: ["@docker"],
  }),
  owner("vercel", "Vercel", "Vercel — frontend cloud platform company behind Next.js.", {
    githubLogins: ["vercel"], scopes: ["@vercel"], names: ["next"],
  }),
  owner("cloudflare", "Cloudflare", "Cloudflare — web infrastructure and security company.", {
    githubLogins: ["cloudflare"], scopes: ["@cloudflare"],
  }),
  owner("alibaba", "Alibaba", "Alibaba — Chinese e-commerce and cloud computing group.", {
    githubLogins: ["alibaba"], groupPrefixes: ["com.alibaba."],
  }),
  owner("tencent", "Tencent", "Tencent — Chinese internet and gaming company behind WeChat.", {
    githubLogins: ["tencent"], scopes: ["@tencent"],
  }),
  owner("bytedance", "ByteDance", "ByteDance — company behind TikTok.", {
    githubLogins: ["bytedance"], scopes: ["@bytedance"],
  }),
  owner("jetbrains", "JetBrains", "JetBrains — maker of IntelliJ IDEA and the Kotlin language.", {
    githubLogins: ["jetbrains"], scopes: ["@jetbrains"], groupPrefixes: ["org.jetbrains."],
  }),
  owner("spotify", "Spotify", "Spotify — music streaming company.", {
    githubLogins: ["spotify"], scopes: ["@spotify"],
  }),
];

/** Whole-token matches only, so `contest` or `latest` are not "test". */
const COMMON_TOKENS: Record<string, true> = Object.fromEntries([
  "jest", "mocha", "jasmine", "karma", "vitest", "cypress", "playwright", "puppeteer", "eslint", "tslint", "stylelint", "prettier",
  "lint", "linter", "test", "tests", "testing", "mock", "mocks", "format", "formatter", "typescript", "sass", "webpack", "rollup",
  "vite", "esbuild", "minify", "minifier", "coverage", "benchmark", "junit", "mockito", "pytest", "flake8", "pylint", "mypy",
  "isort", "checkstyle", "spotbugs", "jacoco",
].map(token => [token, true as const]));
const COMMON_NAMES: Record<string, true> = { black: true, tox: true };

/** Packages whose own name already says what they do. `@types/*` stays a standard package, not a commonly used one. */
export function isCommonlyUsed(node: DepNode): boolean {
  if (node.ecosystem === "project" || node.id.startsWith("npm:@types/")) return false;
  // A Maven group is an organisation, not a description: only the artifact is read.
  const name = (node.ecosystem === "maven" ? node.name.slice(node.name.lastIndexOf(":") + 1) : node.name).toLowerCase();
  return COMMON_NAMES[name] === true || name.split(/[^a-z0-9]+/).some(token => COMMON_TOKENS[token] === true);
}

/** The GitHub login of a `github.com/<login>/…` URL, lower-case, or null. */
export function githubOwnerFromUrl(url: string | null | undefined): string | null {
  if (!url) return null;
  let parsed: URL;
  try { parsed = new URL(url); } catch { return null; }
  if (parsed.hostname !== "github.com" && parsed.hostname !== "www.github.com") return null;
  const login = parsed.pathname.split("/").find(part => part.length > 0)?.toLowerCase().replace(/\.git$/, "");
  return login ? login : null;
}

/** First match wins: npm scope, Maven group, exact name or prefix, then the repository's GitHub login. */
export function knownOwnerOf(node: DepNode, repositoryUrl: string | null | undefined, ownerLogin: string | null | undefined): KnownOwner | null {
  if (node.ecosystem === "project") return null;
  const scope = /^npm:(@[^/]+)\//.exec(node.id)?.[1].toLowerCase();
  const group = /^maven:([^:]+):/.exec(node.id)?.[1].toLowerCase();
  const name = node.name.toLowerCase();
  const login = (ownerLogin ?? githubOwnerFromUrl(repositoryUrl))?.toLowerCase();
  const matchers: ((candidate: KnownOwner) => boolean)[] = [
    candidate => scope !== undefined && candidate.scopes.includes(scope),
    candidate => group !== undefined && candidate.groupPrefixes.some(prefix => group.startsWith(prefix)),
    candidate => candidate.names.includes(name) || candidate.namePrefixes.some(prefix => name.startsWith(prefix)),
    candidate => login !== undefined && candidate.githubLogins.includes(login),
  ];
  for (const matches of matchers) {
    const found = KNOWN_OWNERS.find(matches);
    if (found) return found;
  }
  return null;
}
