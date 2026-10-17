import test from "node:test";
import assert from "node:assert/strict";
import { KNOWN_OWNERS, githubOwnerFromUrl, isCommonlyUsed, knownOwnerOf } from "./known-packages.ts";

const npm = name => ({ id: `npm:${name}`, name, version: null, resolvedVersions: [], declaredVersions: [], ecosystem: "npm", direct: true, dev: false });
const pypi = name => ({ ...npm(name), id: `pypi:${name}`, ecosystem: "pypi" });
const maven = (group, artifact) => ({ ...npm(`${group}:${artifact}`), id: `maven:${group}:${artifact}`, ecosystem: "maven" });
const root = { ...npm("my-app"), id: "project", ecosystem: "project" };

test("names that say what a package does are commonly used in every language", () => {
  for (const name of ["prettier", "jest", "stylelint", "typescript", "@storybook/test-runner", "stylelint-config-standard-scss", "@nx/jest", "copy-webpack-plugin", "lint-staged", "@eslint/js", "sass"]) {
    assert.equal(isCommonlyUsed(npm(name)), true, name);
  }
  assert.equal(isCommonlyUsed(pypi("pytest")), true);
  assert.equal(isCommonlyUsed(pypi("black")), true);
  assert.equal(isCommonlyUsed(maven("org.junit.jupiter", "junit-jupiter")), true);
});

test("packages whose name does not describe them, types, and the project root are not commonly used", () => {
  for (const name of ["husky", "fs-extra", "core-js", "ts-node", "chromatic", "wait-on", "@angular/core", "nx"]) {
    assert.equal(isCommonlyUsed(npm(name)), false, name);
  }
  assert.equal(isCommonlyUsed(npm("@types/jest")), false);
  assert.equal(isCommonlyUsed(root), false);
  // Whole tokens only: "contest" and "latest" are not "test"; a Maven group is not read as a description.
  assert.equal(isCommonlyUsed(npm("contest")), false);
  assert.equal(isCommonlyUsed(npm("latest")), false);
  assert.equal(isCommonlyUsed(maven("org.test.tools", "guava")), false);
  assert.equal(isCommonlyUsed(npm("constructor")), false);
});

test("known owners are found by npm scope, Maven group, exact name, name prefix and repository login", () => {
  assert.equal(knownOwnerOf(npm("@angular/core"), null, null)?.id, "google");
  assert.equal(knownOwnerOf(maven("com.google.guava", "guava"), null, null)?.id, "google");
  assert.equal(knownOwnerOf(npm("typescript"), null, null)?.id, "microsoft");
  assert.equal(knownOwnerOf(pypi("nvidia-cudnn-cu12"), null, null)?.id, "nvidia");
  assert.equal(knownOwnerOf(pypi("boto3"), null, null)?.id, "amazon");
  assert.equal(knownOwnerOf(npm("zx-lib"), "https://github.com/google/zx.git", null)?.id, "google");
  assert.equal(knownOwnerOf(npm("zx-lib"), null, "Netflix")?.id, "netflix");
  assert.equal(knownOwnerOf(npm("somebody"), "https://github.com/someone/else", "someone"), null);
  assert.equal(knownOwnerOf(root, "https://github.com/google/zx", null), null);
});

test("an explicit scope outranks the repository login, and every owner has a one-line description", () => {
  assert.equal(knownOwnerOf(npm("@aws-sdk/client-s3"), "https://github.com/google/other", "google")?.id, "amazon");
  for (const owner of KNOWN_OWNERS) {
    assert.ok(owner.line.startsWith(owner.name) && owner.line.includes(" — "), owner.id);
    assert.ok(!owner.line.includes("\n"), owner.id);
  }
  assert.equal(new Set(KNOWN_OWNERS.map(owner => owner.id)).size, KNOWN_OWNERS.length);
});

test("only github.com repository URLs name an owner", () => {
  assert.equal(githubOwnerFromUrl("https://github.com/Google/zx.git"), "google");
  assert.equal(githubOwnerFromUrl("https://www.github.com/nvidia/cuda"), "nvidia");
  assert.equal(githubOwnerFromUrl("https://github.com/"), null);
  assert.equal(githubOwnerFromUrl("https://gitlab.com/google/zx"), null);
  assert.equal(githubOwnerFromUrl("not a url"), null);
  assert.equal(githubOwnerFromUrl(null), null);
});
