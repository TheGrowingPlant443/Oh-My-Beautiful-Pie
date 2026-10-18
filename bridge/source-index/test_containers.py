"""Svelte / Vue / Astro / EJS embedded-script regions: exact original UTF-8 offsets and scopes."""
from __future__ import annotations

import unittest

import common
import containers
import profiles
import testkit_syntax as kit


def run(lang, source, path=None):
    ext = {"svelte": "svelte", "vue": "vue", "astro": "astro", "ejs": "ejs"}[lang]
    return kit.run(containers, lang, source, path or f"src/Comp.{ext}")


def slice_of(source, fact):
    data = source.encode("utf-8") if isinstance(source, str) else source
    return data[fact["span"]["byteStart"]:fact["span"]["byteEnd"]].decode("utf-8")


def regions(r):
    return {s["region"]: s for s in r["symbols"] if s["kind"] == "module"}


@kit.requires_grammar("javascript")
@kit.requires_grammar("typescript")
class ContainerRecipes(unittest.TestCase):
    def test_profiles_declare_both_grammar_requirements(self):
        for lang in ("svelte", "vue", "astro", "ejs"):
            self.assertEqual(containers.PROFILES[lang]["requires"], ["javascript", "typescript"])
            rec = profiles.probe_profile(lang)
            self.assertTrue(rec["available"], rec["error"])

    def test_svelte_module_and_instance_scopes_with_exact_offsets(self):
        src = (
            "<!-- <script>commented()</script> -->\n"
            "<h1>héllo ✓ {\"<script>fake()</script>\"}</h1>\n"
            "<script context=\"module\" lang=\"ts\">\n"
            "  export const shared = (n: number): number => helper(n);\n"
            "</script>\n"
            "<script>\n"
            "  import Foo from './Foo.svelte';\n"
            "  const label = 'é';\n"
            "  function click() { shared(1); }\n"
            "</script>\n"
            "<style>.a { color: red }</style>\n"
            "<p on:click={click}>{label}</p>\n"
        )
        r = run("svelte", src)
        mods = regions(r)
        self.assertEqual(set(mods), {"module", "instance"})
        self.assertEqual(mods["module"]["language"], "typescript")
        self.assertEqual(mods["instance"]["language"], "javascript")
        self.assertEqual(mods["instance"]["ownerId"], mods["module"]["id"])
        self.assertIsNone(mods["module"]["ownerId"])
        shared = kit.syms(r, "function", "shared")[0]
        self.assertEqual(slice_of(src, shared), "(n: number): number => helper(n)")
        self.assertEqual(shared["region"], "module")
        click = kit.syms(r, "function", "click")[0]
        self.assertEqual(slice_of(src, click), "function click() { shared(1); }")
        self.assertEqual(click["region"], "instance")
        helper = kit.sites(r, "calls", "helper")[0]
        self.assertEqual(slice_of(src, helper), "helper(n)")
        self.assertEqual(helper["language"], "typescript")
        self.assertEqual(slice_of(src, kit.sites(r, "calls", "shared")[0]), "shared(1)")
        names = {s["binding"]["name"] for s in kit.sites(r, "calls")}
        self.assertNotIn("commented", names)
        self.assertNotIn("fake", names)
        imp = kit.sites(r, "imports")[0]
        self.assertEqual((imp["binding"]["specifier"], imp["region"]), ("./Foo.svelte", "instance"))
        # lines/columns are original-file coordinates
        self.assertEqual(shared["span"]["lineStart"], 4)
        self.assertEqual(click["span"]["lineStart"], 9)
        self.assertEqual(click["span"]["columnStart"], 2)
        self.assertTrue(any("template" in d for d in profiles.extract_file("A.svelte", src.encode())["diagnostics"]))
        status = profiles.extract_file("A.svelte", src.encode())["status"]
        self.assertEqual(status, "parsed")

    def test_svelte_nested_script_is_independent_embedded_region(self):
        src = "<script>function a() {}</script>\n<svelte:head><script>function b() { dom(); }</script></svelte:head>\n"
        r = run("svelte", src)
        mods = regions(r)
        self.assertEqual(set(mods), {"instance", "embedded"})
        self.assertIsNone(mods["embedded"]["ownerId"])
        self.assertEqual(slice_of(src, kit.syms(r, "function", "b")[0]), "function b() { dom(); }")

    def test_svelte_unterminated_unsupported_and_data_scripts(self):
        row = profiles.extract_file("U.svelte", b"<script>function a() { }")
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("unterminated" in d for d in row["diagnostics"]))
        row = profiles.extract_file("C.svelte", b"<script lang=\"coffee\">x = 1</script>")
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("coffee" in d for d in row["diagnostics"]))
        row = profiles.extract_file("D.svelte", b"<script type=\"application/json\">{\"a\": 1}</script><script>function ok() {}</script>")
        self.assertEqual(row["status"], "parsed")
        self.assertTrue(any("application/json" in d for d in row["diagnostics"]))
        self.assertEqual([s["name"] for s in row["symbols"] if s["kind"] == "function"], ["ok"])

    def test_vue_setup_and_normal_script_with_template_traps(self):
        src = (
            "<template>\n"
            "  <div>{{ show(\"</script>\") }}</div>\n"
            "  <template v-if=\"ok\"><span/></template>\n"
            "</template>\n"
            "<docs>\n"
            "<script>docs_only()</script>\n"
            "</docs>\n"
            "<script setup lang=\"ts\">\n"
            "import { ref } from 'vue';\n"
            "const r = ref<number>(0);\n"
            "function inc(): void { r.value++; }\n"
            "</script>\n"
            "<script>\n"
            "export default { name: 'ü' };\n"
            "function legacy() { inc(); }\n"
            "</script>\n"
        )
        r = run("vue", src)
        mods = regions(r)
        self.assertEqual(set(mods), {"instance", "module"})
        self.assertEqual(mods["instance"]["language"], "typescript")
        self.assertEqual(mods["module"]["language"], "javascript")
        self.assertEqual(mods["instance"]["ownerId"], mods["module"]["id"])
        names = {s["binding"]["name"] for s in kit.sites(r, "calls")}
        self.assertNotIn("docs_only", names)
        self.assertNotIn("show", names)
        self.assertEqual(slice_of(src, kit.syms(r, "function", "inc")[0]), "function inc(): void { r.value++; }")
        self.assertEqual(slice_of(src, kit.syms(r, "function", "legacy")[0]), "function legacy() { inc(); }")
        self.assertEqual(kit.sites(r, "calls", "ref")[0]["region"], "instance")

    def test_astro_frontmatter_fence_inside_string_and_client_script(self):
        src = (
            "---\n"
            "import Layout from '../Layout.astro';\n"
            "const fence = '---';\n"
            "const tpl = `\n---\n`;\n"
            "const data = load();\n"
            "---\n"
            "<Layout title={\"<script>nope()</script>\"} />\n"
            "<script>\n"
            "  function boot() { init(); }\n"
            "</script>\n"
        )
        r = run("astro", src)
        mods = regions(r)
        self.assertEqual(set(mods), {"instance", "module"})
        self.assertEqual(slice_of(src, kit.sites(r, "calls", "load")[0]), "load()")
        self.assertEqual(slice_of(src, kit.syms(r, "function", "boot")[0]), "function boot() { init(); }")
        self.assertEqual(kit.sites(r, "calls", "load")[0]["region"], "instance")
        self.assertEqual(kit.sites(r, "calls", "init")[0]["region"], "module")
        self.assertNotIn("nope", {s["binding"]["name"] for s in kit.sites(r, "calls")})
        self.assertEqual(kit.sites(r, "imports")[0]["binding"]["specifier"], "../Layout.astro")

    def test_astro_without_closed_frontmatter_is_partial(self):
        row = profiles.extract_file("A.astro", b"---\nconst x = 1;\n<div/>\n")
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("frontmatter" in d for d in row["diagnostics"]))

    def test_ejs_scriptlets_concatenate_and_expressions_are_regions(self):
        src = (
            "<h1><%= title() %></h1>\n"
            "<% if (user) { %>\n"
            "  <p>héllo <%- render(user) %></p>\n"
            "<% } else { fallback(); } %>\n"
            "<%# comment ignored() %>\n"
            "<%% literal <% %>\n"
            "<% const k = 1 -%>\n"
        )
        r = run("ejs", src)
        mods = regions(r)
        self.assertIn("instance", mods)
        self.assertIn("expr#1", mods)
        self.assertIn("expr#2", mods)
        calls = {s["binding"]["name"]: s for s in kit.sites(r, "calls")}
        self.assertEqual(set(calls), {"title", "render", "fallback"})
        for name, text in (("title", "title()"), ("render", "render(user)"), ("fallback", "fallback()")):
            self.assertEqual(slice_of(src, calls[name]), text)
        self.assertEqual(calls["title"]["region"], "expr#1")
        self.assertEqual(calls["fallback"]["region"], "instance")
        self.assertEqual(mods["expr#1"]["ownerId"], mods["instance"]["id"])
        self.assertTrue(calls["fallback"]["conditional"])
        self.assertTrue(any(d["name"] == "k" for d in kit.declarations(r, None) + [d for s in r["symbols"] for d in (s.get("bindings") or {}).get("declarations", [])]))

    def test_ejs_unterminated_tag_is_partial(self):
        row = profiles.extract_file("a.ejs", b"<% fine() %>\n<% never(")
        self.assertEqual(row["status"], "partial")
        self.assertTrue(any("unterminated" in d for d in row["diagnostics"]))
        self.assertIn("fine", [s["binding"]["name"] for s in row["sites"] if s["relation"] == "calls"])

    def test_container_symbol_ids_are_region_scoped_and_deterministic(self):
        src = "<script>function a() {}</script><script context=\"module\">function a() {}</script>"
        a = run("svelte", src)
        b = run("svelte", src)
        self.assertEqual([s["id"] for s in a["symbols"]], [s["id"] for s in b["symbols"]])
        fns = kit.syms(a, "function", "a")
        self.assertEqual(len(fns), 2)
        self.assertNotEqual(fns[0]["id"], fns[1]["id"])
        sp = fns[0]["span"]
        self.assertEqual(
            fns[0]["id"],
            common.symbol_id("src/Comp.svelte", fns[0]["language"], fns[0]["region"], "function_declaration", sp["byteStart"], sp["byteEnd"]),
        )


if __name__ == "__main__":
    unittest.main()
